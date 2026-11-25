//! The local IPC server: newline-delimited JSON frames over a Unix domain socket.
//!
//! Only processes running as the daemon's own user may connect: the socket lives in a
//! private directory (0700), the socket itself is 0600, and every connection's peer
//! credentials are checked. Each client has a bounded outbound queue; a client that
//! cannot keep up with responses is disconnected, and a subscriber that falls behind
//! the event stream gets a `Resync` frame and must take a fresh snapshot.

use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arustex_core::ipc::{
    self, ClientFrame, MAX_FRAME_BYTES, PROTOCOL_VERSION, Request, Response, ServerFrame, WireError,
};
use arustex_core::{CoreError, CoreResult};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::services::{Caller, Services};

/// Stored events replayed per query when a subscriber catches up.
const REPLAY_PAGE: u32 = 1000;
/// A subscriber further behind than this is told to resync instead of replaying.
const MAX_REPLAY: i64 = 50_000;

#[derive(Debug, Clone)]
pub struct ServerSettings {
    pub socket: PathBuf,
    pub max_clients: usize,
    pub client_queue: usize,
    pub request_timeout: Duration,
}

/// A bound listening socket owned by this process.
#[derive(Debug)]
pub struct Server {
    listener: UnixListener,
    settings: ServerSettings,
    uid: u32,
}

/// Creates the socket directory (0700) if needed, refusing directories that other
/// users could write to.
fn prepare_dir(dir: &Path) -> CoreResult<()> {
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| CoreError::invalid(format!("cannot create {}: {e}", dir.display())))?;
    }
    let meta = std::fs::metadata(dir)
        .map_err(|e| CoreError::invalid(format!("cannot inspect {}: {e}", dir.display())))?;
    let own = std::fs::metadata("/proc/self").map(|m| m.uid()).ok();
    if own.is_some_and(|uid| uid != meta.uid()) {
        return Err(CoreError::denied(format!(
            "socket directory {} is owned by another user",
            dir.display()
        )));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(CoreError::denied(format!(
            "socket directory {} is writable by other users",
            dir.display()
        )));
    }
    Ok(())
}

impl Server {
    /// Binds the socket. A live daemon on the same path is an error; a stale socket
    /// file left by a crashed daemon is removed.
    pub async fn bind(settings: ServerSettings) -> CoreResult<Self> {
        let path = &settings.socket;
        let dir = path
            .parent()
            .ok_or_else(|| CoreError::invalid("socket path has no parent directory"))?;
        prepare_dir(dir)?;
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if !meta.file_type().is_socket() {
                return Err(CoreError::invalid(format!(
                    "{} exists and is not a socket",
                    path.display()
                )));
            }
            if UnixStream::connect(path).await.is_ok() {
                return Err(CoreError::Conflict(format!(
                    "a daemon is already listening on {}",
                    path.display()
                )));
            }
            std::fs::remove_file(path).map_err(|e| {
                CoreError::invalid(format!(
                    "cannot remove stale socket {}: {e}",
                    path.display()
                ))
            })?;
        }
        let listener = UnixListener::bind(path)
            .map_err(|e| CoreError::invalid(format!("cannot bind {}: {e}", path.display())))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| CoreError::invalid(format!("cannot restrict {}: {e}", path.display())))?;
        let uid = std::fs::metadata(path)
            .map_err(|e| CoreError::invalid(format!("cannot inspect {}: {e}", path.display())))?
            .uid();
        Ok(Self {
            listener,
            settings,
            uid,
        })
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn socket(&self) -> &Path {
        &self.settings.socket
    }

    /// Accepts connections until `shutdown`, then closes them and removes the socket.
    pub async fn serve(self, services: Arc<Services>, shutdown: CancellationToken) {
        let slots = Arc::new(Semaphore::new(self.settings.max_clients.max(1)));
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                accepted = self.listener.accept() => {
                    let (stream, _) = match accepted {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(error = %e, "accept failed");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    let peer = match stream.peer_cred() {
                        Ok(c) => c.uid(),
                        Err(e) => {
                            tracing::warn!(error = %e, "cannot read peer credentials; closing");
                            continue;
                        }
                    };
                    if peer != self.uid {
                        tracing::warn!(peer_uid = peer, "rejected connection from another user");
                        continue;
                    }
                    let Ok(permit) = slots.clone().try_acquire_owned() else {
                        tracing::warn!("client limit reached; closing connection");
                        reject_busy(stream).await;
                        continue;
                    };
                    let conn = Connection {
                        services: services.clone(),
                        caller: Caller { uid: peer },
                        settings: self.settings.clone(),
                        cancel: shutdown.child_token(),
                    };
                    connections.spawn(async move {
                        let _permit = permit;
                        conn.run(stream).await;
                    });
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        connections.shutdown().await;
        let _ = std::fs::remove_file(&self.settings.socket);
    }
}

/// Reads one newline-terminated frame of at most [`MAX_FRAME_BYTES`] (plus one byte, so
/// callers can detect oversized frames).
pub(crate) async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    let mut limited = (&mut *reader).take(MAX_FRAME_BYTES as u64 + 1);
    limited.read_until(b'\n', buf).await
}

async fn reject_busy(mut stream: UnixStream) {
    let frame = ServerFrame::Response {
        v: PROTOCOL_VERSION,
        id: 0,
        result: Err(CoreError::Unavailable("too many clients".into()).into()),
    };
    if let Ok(line) = ipc::encode(&frame) {
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            let _ = stream.write_all(line.as_bytes()).await;
            let _ = stream.write_all(b"\n").await;
        })
        .await;
    }
}

struct Connection {
    services: Arc<Services>,
    caller: Caller,
    settings: ServerSettings,
    cancel: CancellationToken,
}

/// The sending half of a client's bounded outbound queue.
#[derive(Clone)]
struct Outbox {
    tx: mpsc::Sender<String>,
    cancel: CancellationToken,
    timeout: Duration,
}

impl Outbox {
    /// Queues a response; a client that does not drain its queue in time is dropped.
    async fn respond(&self, id: u64, result: Result<Response, WireError>) {
        let frame = ServerFrame::Response {
            v: PROTOCOL_VERSION,
            id,
            result,
        };
        let line = match ipc::encode(&frame) {
            Ok(l) => l,
            Err(e) => {
                let fallback = ServerFrame::Response {
                    v: PROTOCOL_VERSION,
                    id,
                    result: Err(e.into()),
                };
                match ipc::encode(&fallback) {
                    Ok(l) => l,
                    Err(_) => return,
                }
            }
        };
        if tokio::time::timeout(self.timeout, self.tx.send(line))
            .await
            .map_or(true, |r| r.is_err())
        {
            tracing::warn!("client did not drain responses; disconnecting");
            self.cancel.cancel();
        }
    }

    /// Queues an event without waiting. Returns false when the queue is full.
    fn try_event(&self, frame: &ServerFrame) -> bool {
        match ipc::encode(frame) {
            Ok(line) => self.tx.try_send(line).is_ok(),
            Err(e) => {
                tracing::warn!(error = %e, "dropping unencodable event");
                true
            }
        }
    }
}

impl Connection {
    async fn run(self, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<String>(self.settings.client_queue.max(8));
        let outbox = Outbox {
            tx,
            cancel: self.cancel.clone(),
            timeout: self.settings.request_timeout,
        };
        let writer_cancel = self.cancel.clone();
        let writer = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = writer_cancel.cancelled() => break,
                    line = rx.recv() => {
                        let Some(line) = line else { break };
                        let ok = write.write_all(line.as_bytes()).await.is_ok()
                            && write.write_all(b"\n").await.is_ok();
                        if !ok {
                            writer_cancel.cancel();
                            break;
                        }
                    }
                }
            }
            let _ = write.shutdown().await;
        });

        let mut reader = BufReader::new(read);
        let mut buf = Vec::new();
        let mut subscription: Option<JoinHandle<()>> = None;
        loop {
            buf.clear();
            let read = tokio::select! {
                () = self.cancel.cancelled() => break,
                r = read_frame(&mut reader, &mut buf) => r,
            };
            match read {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if buf.len() > MAX_FRAME_BYTES {
                outbox
                    .respond(
                        0,
                        Err(CoreError::LimitExceeded("frame too large".into()).into()),
                    )
                    .await;
                break;
            }
            let Ok(text) = std::str::from_utf8(&buf) else {
                outbox
                    .respond(0, Err(CoreError::invalid("frame is not UTF-8").into()))
                    .await;
                break;
            };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let ClientFrame { id, request, .. } = match ipc::decode_client(text) {
                Ok(f) => f,
                Err(e) => {
                    // Malformed frames are answered once; the connection stays usable.
                    outbox.respond(0, Err(e.into())).await;
                    continue;
                }
            };
            tracing::debug!(id, method = method_name(&request), "request");
            if let Request::Subscribe { after_seq } = request {
                if let Some(old) = subscription.take() {
                    old.abort();
                }
                subscription = Some(tokio::spawn(subscribe(
                    self.services.clone(),
                    outbox.clone(),
                    id,
                    after_seq,
                )));
                continue;
            }
            let result = tokio::time::timeout(
                self.settings.request_timeout,
                self.services.handle(&self.caller, request),
            )
            .await
            .unwrap_or_else(|_| Err(CoreError::Timeout("request timed out".into())));
            if let Err(e) = &result {
                tracing::debug!(id, error = %e, "request failed");
            }
            outbox.respond(id, result.map_err(WireError::from)).await;
        }
        if let Some(s) = subscription {
            s.abort();
        }
        self.cancel.cancel();
        let _ = writer.await;
    }
}

/// Streams events to one client: replays stored events after `after_seq`, then
/// forwards live events without gaps or duplicates.
async fn subscribe(services: Arc<Services>, outbox: Outbox, id: u64, after_seq: i64) {
    let store = services.store().clone();
    // Subscribe before reading the stored tail so nothing falls in between.
    let mut live = store.subscribe_live();
    let latest = match store.latest_event_seq().await {
        Ok(s) => s,
        Err(e) => {
            outbox.respond(id, Err(e.into())).await;
            return;
        }
    };
    let resync = |latest_seq| ServerFrame::Resync {
        v: PROTOCOL_VERSION,
        latest_seq,
    };
    outbox
        .respond(id, Ok(Response::Subscribed { latest_seq: latest }))
        .await;
    let mut last = after_seq.clamp(0, latest);
    if latest - last > MAX_REPLAY {
        let _ = outbox.try_event(&resync(latest));
        return;
    }
    while last < latest {
        let page = match store.events_after(last, REPLAY_PAGE).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "event replay failed");
                let _ = outbox.try_event(&resync(latest));
                return;
            }
        };
        if page.is_empty() {
            break;
        }
        for event in page {
            last = event.seq;
            if !outbox.try_event(&ServerFrame::Event {
                v: PROTOCOL_VERSION,
                event,
            }) {
                lagged(&outbox, &store).await;
                return;
            }
        }
    }
    loop {
        match live.recv().await {
            Ok(event) => {
                if event.seq != 0 {
                    if event.seq <= last {
                        continue;
                    }
                    last = event.seq;
                }
                if !outbox.try_event(&ServerFrame::Event {
                    v: PROTOCOL_VERSION,
                    event,
                }) {
                    lagged(&outbox, &store).await;
                    return;
                }
            }
            Err(RecvError::Lagged(_)) => {
                lagged(&outbox, &store).await;
                return;
            }
            Err(RecvError::Closed) => return,
        }
    }
}

/// Tells a subscriber that fell behind to resync, waiting briefly for queue space.
async fn lagged(outbox: &Outbox, store: &arustex_store::Store) {
    let latest = store.latest_event_seq().await.unwrap_or(0);
    let frame = ServerFrame::Resync {
        v: PROTOCOL_VERSION,
        latest_seq: latest,
    };
    let Ok(line) = ipc::encode(&frame) else {
        return;
    };
    if tokio::time::timeout(outbox.timeout, outbox.tx.send(line))
        .await
        .map_or(true, |r| r.is_err())
    {
        outbox.cancel.cancel();
    }
}

fn method_name(request: &Request) -> &'static str {
    match request {
        Request::Hello { .. } => "hello",
        Request::Ping => "ping",
        Request::Dashboard => "dashboard",
        Request::ListTasks { .. } => "list_tasks",
        Request::GetTask { .. } => "get_task",
        Request::CreateTask { .. } => "create_task",
        Request::ControlTask { .. } => "control_task",
        Request::ListSessions { .. } => "list_sessions",
        Request::GetTranscript { .. } => "get_transcript",
        Request::ListApprovals { .. } => "list_approvals",
        Request::DecideApproval { .. } => "decide_approval",
        Request::ListAssets { .. } => "list_assets",
        Request::GetAsset { .. } => "get_asset",
        Request::ListDatasets { .. } => "list_datasets",
        Request::ListRecords { .. } => "list_records",
        Request::GetRecord { .. } => "get_record",
        Request::ListFindings { .. } => "list_findings",
        Request::SetFindingStatus { .. } => "set_finding_status",
        Request::ListReviewResults { .. } => "list_review_results",
        Request::DecideReviewResult { .. } => "decide_review_result",
        Request::AddNote { .. } => "add_note",
        Request::ListNotes { .. } => "list_notes",
        Request::ListReports { .. } => "list_reports",
        Request::GetReport { .. } => "get_report",
        Request::GenerateReport { .. } => "generate_report",
        Request::GetSettings => "get_settings",
        Request::AssistantMessage { .. } => "assistant_message",
        Request::Subscribe { .. } => "subscribe",
    }
}
