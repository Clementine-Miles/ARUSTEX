//! IPC client used by the TUI and the command line.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arustex_core::events::EventEnvelope;
use arustex_core::ipc::{
    self, ClientFrame, MAX_FRAME_BYTES, PROTOCOL_VERSION, Request, Response, ServerFrame, WireError,
};
use arustex_core::{CoreError, CoreResult};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Something the daemon pushed without a request.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    Event(EventEnvelope),
    /// Events were dropped; take a fresh snapshot and subscribe from `latest_seq`.
    Resync {
        latest_seq: i64,
    },
    /// The connection closed. Reconnect with [`Client::connect`].
    Disconnected,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Response, WireError>>>>>;

#[derive(Debug)]
pub struct Client {
    tx: mpsc::Sender<String>,
    pending: Pending,
    next_id: AtomicU64,
    timeout: Duration,
    closed: CancellationToken,
    pub server: String,
    pub socket: PathBuf,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

/// Refuses sockets that are not owned by the current user, so a client never talks
/// to a daemon (or an impostor) running as someone else.
fn check_socket_owner(path: &Path) -> CoreResult<()> {
    let meta = std::fs::metadata(path).map_err(|e| {
        CoreError::Unavailable(format!(
            "cannot reach the daemon at {}: {e} (is `arustex daemon` running?)",
            path.display()
        ))
    })?;
    let own = std::fs::metadata("/proc/self").map(|m| m.uid()).ok();
    if own.is_some_and(|uid| uid != meta.uid()) {
        return Err(CoreError::denied(format!(
            "{} is owned by another user",
            path.display()
        )));
    }
    Ok(())
}

/// Delivers an event, or after an overflow a single `Resync` once there is room again.
fn push_event(
    events: &mpsc::Sender<ClientEvent>,
    event: ClientEvent,
    last_seq: i64,
    overflow: &mut bool,
) {
    if *overflow {
        if events
            .try_send(ClientEvent::Resync {
                latest_seq: last_seq,
            })
            .is_ok()
        {
            *overflow = false;
        }
        return;
    }
    if events.try_send(event).is_err() {
        *overflow = true;
    }
}

impl Client {
    /// Connects, performs the hello handshake and returns the client with its event
    /// stream. `events_capacity` bounds buffered events; on overflow a `Resync` is
    /// delivered instead of the dropped events.
    pub async fn connect(
        socket: &Path,
        client_name: &str,
        timeout: Duration,
        events_capacity: usize,
    ) -> CoreResult<(Self, mpsc::Receiver<ClientEvent>)> {
        check_socket_owner(socket)?;
        let stream = tokio::time::timeout(timeout, UnixStream::connect(socket))
            .await
            .map_err(|_| CoreError::Timeout("connecting to the daemon timed out".into()))?
            .map_err(|e| {
                CoreError::Unavailable(format!(
                    "cannot connect to {}: {e} (is `arustex daemon` running?)",
                    socket.display()
                ))
            })?;
        let (read, mut write) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<String>(256);
        let (events_tx, events_rx) = mpsc::channel(events_capacity.max(16));
        let pending: Pending = Arc::default();
        let closed = CancellationToken::new();

        let writer_closed = closed.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = writer_closed.cancelled() => break,
                    line = rx.recv() => {
                        let Some(line) = line else { break };
                        if write.write_all(line.as_bytes()).await.is_err()
                            || write.write_all(b"\n").await.is_err()
                        {
                            writer_closed.cancel();
                            break;
                        }
                    }
                }
            }
        });

        let reader_pending = pending.clone();
        let reader_closed = closed.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(read);
            let mut buf = Vec::new();
            let mut overflow = false;
            let mut last_seq = 0i64;
            loop {
                buf.clear();
                let r = tokio::select! {
                    () = reader_closed.cancelled() => break,
                    r = crate::server::read_frame(&mut reader, &mut buf) => r,
                };
                match r {
                    Ok(0) | Err(_) => break,
                    Ok(_) if buf.len() > MAX_FRAME_BYTES => break,
                    Ok(_) => {}
                }
                let Ok(text) = std::str::from_utf8(&buf) else {
                    break;
                };
                let frame = match ipc::decode_server(text.trim()) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(error = %e, "ignoring malformed frame from daemon");
                        continue;
                    }
                };
                match frame {
                    ServerFrame::Response { id, result, .. } => {
                        let waiter = reader_pending.lock().expect("lock").remove(&id);
                        match waiter {
                            Some(w) => {
                                let _ = w.send(result);
                            }
                            None => {
                                if let Err(e) = result {
                                    tracing::debug!(id, error = %e.message, "unsolicited error from daemon");
                                }
                            }
                        }
                    }
                    ServerFrame::Event { event, .. } => {
                        if event.seq > last_seq {
                            last_seq = event.seq;
                        }
                        push_event(
                            &events_tx,
                            ClientEvent::Event(event),
                            last_seq,
                            &mut overflow,
                        );
                    }
                    ServerFrame::Resync { latest_seq, .. } => {
                        overflow = false;
                        let _ = events_tx.try_send(ClientEvent::Resync { latest_seq });
                    }
                }
            }
            reader_closed.cancel();
            reader_pending.lock().expect("lock").clear();
            let _ = events_tx.send(ClientEvent::Disconnected).await;
        });

        let mut client = Self {
            tx,
            pending,
            next_id: AtomicU64::new(1),
            timeout,
            closed,
            server: String::new(),
            socket: socket.to_owned(),
        };
        match client
            .request(Request::Hello {
                client: client_name.to_owned(),
            })
            .await?
        {
            Response::Hello {
                protocol, server, ..
            } if protocol == PROTOCOL_VERSION => client.server = server,
            Response::Hello { protocol, .. } => {
                return Err(CoreError::invalid(format!(
                    "daemon speaks protocol {protocol}, this client {PROTOCOL_VERSION}"
                )));
            }
            other => {
                return Err(CoreError::Internal(format!(
                    "unexpected handshake reply: {other:?}"
                )));
            }
        }
        Ok((client, events_rx))
    }

    pub fn is_connected(&self) -> bool {
        !self.closed.is_cancelled()
    }

    /// Sends a request and waits for its response.
    pub async fn request(&self, request: Request) -> CoreResult<Response> {
        if self.closed.is_cancelled() {
            return Err(CoreError::Unavailable("not connected to the daemon".into()));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let line = ipc::encode(&ClientFrame {
            v: PROTOCOL_VERSION,
            id,
            request,
        })?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("lock").insert(id, tx);
        let cleanup = || {
            self.pending.lock().expect("lock").remove(&id);
        };
        if self.tx.send(line).await.is_err() {
            cleanup();
            return Err(CoreError::Unavailable("connection closed".into()));
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(result)) => result.map_err(CoreError::from),
            Ok(Err(_)) => Err(CoreError::Unavailable("connection closed".into())),
            Err(_) => {
                cleanup();
                Err(CoreError::Timeout(
                    "the daemon did not answer in time".into(),
                ))
            }
        }
    }
}
