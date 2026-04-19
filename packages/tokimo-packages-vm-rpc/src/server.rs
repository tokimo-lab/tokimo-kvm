//! Host-side RPC server: drives any `AsyncRead + AsyncWrite` duplex.
//!
//! Backends are responsible for accepting connections on their chosen
//! transport (Unix socket, named pipe, FD pair) and calling
//! [`serve_stream`] once per accepted connection.

use crate::frame::{read_frame, write_frame};
use crate::proto::{Request, Response, PROTOCOL_VERSION};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokimo_packages_vm_core::TokimoVfs;

/// Serve exactly one connection to completion.
pub async fn serve_stream<S>(stream: S, vfs: Arc<dyn TokimoVfs>) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut r, mut w) = tokio::io::split(stream);
    loop {
        let req: Request = match read_frame(&mut r).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("rpc read: {e}");
                return Ok(());
            }
        };
        let resp = handle(&*vfs, req).await;
        if let Err(e) = write_frame(&mut w, &resp).await {
            tracing::debug!("rpc write: {e}");
            return Ok(());
        }
    }
}

async fn handle(vfs: &dyn TokimoVfs, req: Request) -> Response {
    match req {
        Request::Hello { .. } => Response::Hello { protocol_version: PROTOCOL_VERSION },
        Request::Stat { path } => Response::Stat(vfs.stat(&path).await),
        Request::List { path } => Response::List(vfs.list(&path).await),
        Request::Read { path, offset, len } => Response::Read(vfs.read(&path, offset, len).await),
        Request::Write { path, offset, data } => Response::Write(vfs.write(&path, offset, &data).await),
        Request::Create { path, mode } => Response::Create(vfs.create(&path, mode).await),
        Request::Mkdir { path, mode } => Response::Mkdir(vfs.mkdir(&path, mode).await),
        Request::Remove { path } => Response::Remove(vfs.remove(&path).await),
        Request::Rmdir { path } => Response::Rmdir(vfs.rmdir(&path).await),
        Request::Rename { from, to } => Response::Rename(vfs.rename(&from, &to).await),
        Request::Truncate { path, size } => Response::Truncate(vfs.truncate(&path, size).await),
    }
}
