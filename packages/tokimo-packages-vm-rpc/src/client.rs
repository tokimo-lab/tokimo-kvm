//! Guest-side RPC client. One connection serves many calls; requests are
//! serialised with a mutex so the single virtio-serial port is never
//! interleaved.

use crate::frame::{read_frame, write_frame};
use crate::proto::{Request, Response, PROTOCOL_VERSION};
use std::path::Path;
#[cfg(unix)]
use std::time::Duration;
use tokimo_packages_vm_core::{DirEntry, FileAttr, VfsError, VfsResult};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::sync::Mutex;

/// Boxed duplex transport. Only used internally by [`Client`].
pub trait Transport: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Transport for T {}

type IoPair = (ReadHalf<Box<dyn Transport>>, WriteHalf<Box<dyn Transport>>);

pub struct Client {
    io: Mutex<IoPair>,
}

impl Client {
    /// Build a client directly from an existing duplex stream (mostly for tests).
    pub fn from_stream<S: Transport>(s: S) -> Self {
        let boxed: Box<dyn Transport> = Box::new(s);
        let (r, w) = tokio::io::split(boxed);
        Self {
            io: Mutex::new((r, w)),
        }
    }

    /// Like [`Self::from_stream`] but also performs the opening handshake.
    pub async fn handshake_from_stream<S: Transport>(s: S) -> anyhow::Result<Self> {
        let me = Self::from_stream(s);
        me.hello().await?;
        Ok(me)
    }

    /// Open a virtio-serial port inside a Linux guest.
    ///
    /// `port_name` is either the bare port name (e.g. `tokimo.rpc.mem`,
    /// looked up under `/dev/virtio-ports/`) or an absolute path to a
    /// character device. Waits up to 30s for the node to appear (udev
    /// lag on early boot).
    #[cfg(unix)]
    pub async fn connect_port(port_name: &str) -> anyhow::Result<Self> {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let path = loop {
            if let Some(p) = resolve_virtio_port(port_name)? {
                break p;
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("virtio-serial port {port_name} never appeared");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(&path)
            .await?;
        let me = Self::from_stream(file);
        me.hello().await?;
        Ok(me)
    }

    async fn call(&self, req: Request) -> anyhow::Result<Response> {
        let mut g = self.io.lock().await;
        let (r, w) = &mut *g;
        write_frame(w, &req).await?;
        let resp: Response = read_frame(r).await?;
        Ok(resp)
    }

    pub async fn hello(&self) -> anyhow::Result<()> {
        match self
            .call(Request::Hello {
                protocol_version: PROTOCOL_VERSION,
            })
            .await?
        {
            Response::Hello { protocol_version } if protocol_version == PROTOCOL_VERSION => Ok(()),
            Response::Hello { protocol_version } => {
                anyhow::bail!("protocol mismatch: server {protocol_version}")
            }
            _ => anyhow::bail!("unexpected response"),
        }
    }

    fn io_err<E: std::fmt::Display>(e: E) -> VfsError {
        VfsError::Io(e.to_string())
    }

    pub async fn stat(&self, path: &Path) -> VfsResult<FileAttr> {
        match self
            .call(Request::Stat {
                path: path.to_path_buf(),
            })
            .await
            .map_err(Self::io_err)?
        {
            Response::Stat(r) => r,
            _ => Err(VfsError::Io("bad resp".into())),
        }
    }
    pub async fn list(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        match self
            .call(Request::List {
                path: path.to_path_buf(),
            })
            .await
            .map_err(Self::io_err)?
        {
            Response::List(r) => r,
            _ => Err(VfsError::Io("bad resp".into())),
        }
    }
    pub async fn read(&self, path: &Path, offset: u64, len: u32) -> VfsResult<Vec<u8>> {
        match self
            .call(Request::Read {
                path: path.to_path_buf(),
                offset,
                len,
            })
            .await
            .map_err(Self::io_err)?
        {
            Response::Read(r) => r,
            _ => Err(VfsError::Io("bad resp".into())),
        }
    }
    pub async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        match self
            .call(Request::Write {
                path: path.to_path_buf(),
                offset,
                data: data.to_vec(),
            })
            .await
            .map_err(Self::io_err)?
        {
            Response::Write(r) => r,
            _ => Err(VfsError::Io("bad resp".into())),
        }
    }
    pub async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        match self
            .call(Request::Create {
                path: path.to_path_buf(),
                mode,
            })
            .await
            .map_err(Self::io_err)?
        {
            Response::Create(r) => r,
            _ => Err(VfsError::Io("bad resp".into())),
        }
    }
    pub async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        match self
            .call(Request::Mkdir {
                path: path.to_path_buf(),
                mode,
            })
            .await
            .map_err(Self::io_err)?
        {
            Response::Mkdir(r) => r,
            _ => Err(VfsError::Io("bad resp".into())),
        }
    }
    pub async fn remove(&self, path: &Path) -> VfsResult<()> {
        match self
            .call(Request::Remove {
                path: path.to_path_buf(),
            })
            .await
            .map_err(Self::io_err)?
        {
            Response::Remove(r) => r,
            _ => Err(VfsError::Io("bad resp".into())),
        }
    }
    pub async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        match self
            .call(Request::Rmdir {
                path: path.to_path_buf(),
            })
            .await
            .map_err(Self::io_err)?
        {
            Response::Rmdir(r) => r,
            _ => Err(VfsError::Io("bad resp".into())),
        }
    }
    pub async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        match self
            .call(Request::Rename {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
            })
            .await
            .map_err(Self::io_err)?
        {
            Response::Rename(r) => r,
            _ => Err(VfsError::Io("bad resp".into())),
        }
    }
    pub async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        match self
            .call(Request::Truncate {
                path: path.to_path_buf(),
                size,
            })
            .await
            .map_err(Self::io_err)?
        {
            Response::Truncate(r) => r,
            _ => Err(VfsError::Io("bad resp".into())),
        }
    }
}

/// Locate the `/dev/vportXpY` character device for a virtio-serial port
/// with the given name. The `/dev/virtio-ports/<name>` symlinks are set
/// up by udev, which may not be running in a minimal guest; this falls
/// back to iterating `/sys/class/virtio-ports/*/name`.
#[cfg(unix)]
fn resolve_virtio_port(port_name: &str) -> anyhow::Result<Option<std::path::PathBuf>> {
    if port_name.starts_with('/') {
        let p = std::path::PathBuf::from(port_name);
        return Ok(if p.exists() { Some(p) } else { None });
    }
    // 1. Udev-created symlink.
    let by_name = std::path::PathBuf::from(format!("/dev/virtio-ports/{port_name}"));
    if by_name.exists() {
        return Ok(Some(by_name));
    }
    // 2. Fallback: scan sysfs.
    let root = std::path::Path::new("/sys/class/virtio-ports");
    if !root.exists() {
        return Ok(None);
    }
    for entry in std::fs::read_dir(root)? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name_path = entry.path().join("name");
        let name = match std::fs::read_to_string(&name_path) {
            Ok(s) => s.trim().to_string(),
            Err(_) => continue,
        };
        if name == port_name {
            let dev =
                std::path::PathBuf::from(format!("/dev/{}", entry.file_name().to_string_lossy()));
            if dev.exists() {
                return Ok(Some(dev));
            }
        }
    }
    Ok(None)
}
