use std::{
    fs,
    io::{self, ErrorKind},
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use arustex_core::{ErrorCode, PROTOCOL_VERSION, Query, Reply, Request, Response};
use arustex_store::Store;
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    signal::unix::{SignalKind, signal},
    task::JoinSet,
    time::timeout,
};

const REQUEST_BYTES: usize = 4 * 1024;
const RESPONSE_BYTES: usize = 512 * 1024;
const MAX_CLIENTS: usize = 16;
const DEADLINE: Duration = Duration::from_secs(8);

pub async fn serve(path: &Path, store: Store) -> io::Result<()> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let (path, uid) = socket_path(path)?;
    let listener = UnixListener::bind(&path).map_err(|error| {
        if error.kind() == ErrorKind::AddrInUse {
            io::Error::new(
                ErrorKind::AddrInUse,
                "socket path is in use; stop the daemon before removing a stale socket",
            )
        } else {
            error
        }
    })?;
    let socket = OwnedSocket::track(path)?;
    fs::set_permissions(&socket.path, fs::Permissions::from_mode(0o600))?;
    let mut handlers = JoinSet::new();
    loop {
        tokio::select! {
            _ = interrupt.recv() => break,
            _ = terminate.recv() => break,
            _ = handlers.join_next(), if !handlers.is_empty() => {},
            client = listener.accept(), if handlers.len() < MAX_CLIENTS => {
                let (stream, _) = client?;
                if verify_peer(&stream, uid).is_ok() {
                    let store = store.clone();
                    handlers.spawn(async move {
                        let _ = timeout(DEADLINE, handle_client(stream, &store)).await;
                    });
                }
            }
        }
    }
    drop(listener);
    handlers.shutdown().await;
    Ok(())
}

pub async fn request(path: &Path, query: Query) -> io::Result<Reply> {
    let (path, uid) = socket_path(path)?;
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "socket must be owned by the current user with owner-only permissions",
        ));
    }
    timeout(DEADLINE, async move {
        let mut stream = UnixStream::connect(path).await?;
        verify_peer(&stream, uid)?;
        write_frame(
            &mut stream,
            &Request {
                version: PROTOCOL_VERSION,
                query,
            },
            REQUEST_BYTES,
        )
        .await?;
        let response: Response = read_frame(&mut stream, RESPONSE_BYTES).await?;
        if response.version != PROTOCOL_VERSION {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "incompatible daemon protocol version",
            ));
        }
        Ok(response.reply)
    })
    .await
    .map_err(|_| io::Error::new(ErrorKind::TimedOut, "local request timed out"))?
}

async fn handle_client(mut stream: UnixStream, store: &Store) -> io::Result<()> {
    let reply = match read_frame::<Request>(&mut stream, REQUEST_BYTES).await {
        Err(error) if error.kind() == ErrorKind::InvalidData => Reply::Error {
            code: ErrorCode::InvalidRequest,
        },
        Err(error) => return Err(error),
        Ok(request) if request.version != PROTOCOL_VERSION => Reply::Error {
            code: ErrorCode::IncompatibleVersion,
        },
        Ok(request) => match request.query.validate() {
            Err(code) => Reply::Error { code },
            Ok(()) => match request.query {
                Query::Ping => Reply::Pong,
                Query::Browse {
                    view,
                    before_id,
                    limit,
                } => match store.browse(view, before_id, limit).await {
                    Ok(page) => Reply::Page(page),
                    Err(_) => Reply::Error {
                        code: ErrorCode::StoreUnavailable,
                    },
                },
            },
        },
    };
    write_frame(
        &mut stream,
        &Response {
            version: PROTOCOL_VERSION,
            reply,
        },
        RESPONSE_BYTES,
    )
    .await
}

async fn read_frame<T: DeserializeOwned>(stream: &mut UnixStream, bound: usize) -> io::Result<T> {
    let size = stream.read_u32().await? as usize;
    if !(1..=bound).contains(&size) {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "invalid IPC frame length",
        ));
    }
    let mut frame = vec![0; size];
    stream.read_exact(&mut frame).await?;
    serde_json::from_slice(&frame)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "invalid IPC frame"))
}

async fn write_frame<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
    bound: usize,
) -> io::Result<()> {
    let frame = serde_json::to_vec(value)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "cannot serialize IPC frame"))?;
    if frame.len() > bound {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "IPC frame exceeds its size limit",
        ));
    }
    stream.write_u32(frame.len() as u32).await?;
    stream.write_all(&frame).await
}

fn socket_path(path: &Path) -> io::Result<(PathBuf, u32)> {
    let parent = path
        .parent()
        .filter(|parent| parent.is_absolute())
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "socket path needs an absolute parent directory",
            )
        })?;
    let meta = fs::symlink_metadata(parent)?;
    let uid = rustix::process::geteuid().as_raw();
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "socket parent must be owned by the current user and inaccessible to others",
        ));
    }
    Ok((path.to_owned(), uid))
}

fn verify_peer(stream: &UnixStream, uid: u32) -> io::Result<()> {
    if stream.peer_cred()?.uid() == uid {
        Ok(())
    } else {
        Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "IPC peer has a different effective user",
        ))
    }
}

struct OwnedSocket {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl OwnedSocket {
    fn track(path: PathBuf) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_socket() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "socket path is not a socket",
            ));
        }
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for OwnedSocket {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_over_a_local_socket() {
        let (mut first, mut second) = UnixStream::pair().unwrap();
        let uid = rustix::process::geteuid().as_raw();
        assert!(verify_peer(&first, uid).is_ok());
        assert!(verify_peer(&first, uid.wrapping_add(1)).is_err());

        let expected = Request {
            version: PROTOCOL_VERSION,
            query: Query::Ping,
        };
        write_frame(&mut first, &expected, REQUEST_BYTES)
            .await
            .unwrap();
        assert_eq!(
            read_frame::<Request>(&mut second, REQUEST_BYTES)
                .await
                .unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn frame_length_is_checked_before_reading_the_payload() {
        let (mut first, mut second) = UnixStream::pair().unwrap();
        first.write_u32(REQUEST_BYTES as u32 + 1).await.unwrap();
        let error = read_frame::<Request>(&mut second, REQUEST_BYTES)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }
}
