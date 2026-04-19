//! tokimo-core v2: cross-platform sandbox traits and types.
//!
//! Platform-independent. No FUSE, no hypervisor deps. The [`TokimoVfs`]
//! trait is the user-supplied filesystem; backends ship it to the guest
//! over an RPC transport (vsock or TCP via user-mode networking).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

// ---------------------------------------------------------------- VFS

/// Kind of filesystem entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsKind { File, Dir, Symlink }

/// Minimal attributes the agent needs to fill a FUSE `stat`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAttr {
    pub size: u64,
    pub mode: u32,
    pub kind: FsKind,
    pub mtime_secs: i64,
    pub atime_secs: i64,
    pub ctime_secs: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub kind: FsKind,
}

#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum VfsError {
    #[error("not found")]
    NotFound,
    #[error("permission denied")]
    PermissionDenied,
    #[error("already exists")]
    AlreadyExists,
    #[error("not a directory")]
    NotADirectory,
    #[error("is a directory")]
    IsADirectory,
    #[error("io: {0}")]
    Io(String),
}

pub type VfsResult<T> = std::result::Result<T, VfsError>;

/// User-supplied filesystem. Backends never mount this on the host;
/// instead it's served via RPC to a `tokimo-agent` FUSE mount in the guest.
#[async_trait]
pub trait TokimoVfs: Send + Sync + 'static {
    async fn stat(&self, path: &Path) -> VfsResult<FileAttr>;
    async fn list(&self, path: &Path) -> VfsResult<Vec<DirEntry>>;
    async fn read(&self, path: &Path, offset: u64, len: u32) -> VfsResult<Vec<u8>>;
    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32>;
    async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr>;
    async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr>;
    async fn remove(&self, path: &Path) -> VfsResult<()>;
    async fn rmdir(&self, path: &Path) -> VfsResult<()>;
    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()>;
    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()>;
}

// ---------------------------------------------------------------- HostFs adapter

/// TokimoVfs implementation backed by a real host directory.
///
/// Used internally by backends to route `MountSpec::HostDir` through the
/// same virtio-serial RPC transport as `MountSpec::Vfs`. This removes the
/// need for 9p/virtiofs kernel modules in the guest image.
pub struct HostFs {
    root: PathBuf,
    read_only: bool,
}

impl HostFs {
    pub fn new(root: impl Into<PathBuf>, read_only: bool) -> Self {
        Self { root: root.into(), read_only }
    }

    fn resolve(&self, p: &Path) -> VfsResult<PathBuf> {
        let rel = p.strip_prefix("/").unwrap_or(p);
        let joined = self.root.join(rel);
        // Basic jail check against ".." escapes. We require the resolved
        // path to be underneath `self.root`.
        let canon_root = std::fs::canonicalize(&self.root)
            .map_err(|e| VfsError::Io(e.to_string()))?;
        // It's fine if the path itself does not yet exist (create/mkdir).
        let probe = joined.parent().unwrap_or(&joined);
        let canon_parent = std::fs::canonicalize(probe)
            .unwrap_or_else(|_| probe.to_path_buf());
        if !canon_parent.starts_with(&canon_root) {
            return Err(VfsError::PermissionDenied);
        }
        Ok(joined)
    }

    fn attr_from_meta(m: &std::fs::Metadata) -> FileAttr {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        let kind = if m.is_dir() { FsKind::Dir }
            else if m.file_type().is_symlink() { FsKind::Symlink }
            else { FsKind::File };
        let to_secs = |t: std::io::Result<std::time::SystemTime>| -> i64 {
            t.ok()
                .and_then(|v| v.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        };
        FileAttr {
            size: m.len(),
            mode: {
                #[cfg(unix)] { m.mode() }
                #[cfg(not(unix))] { if m.is_dir() { 0o040755 } else { 0o100644 } }
            },
            kind,
            mtime_secs: to_secs(m.modified()),
            atime_secs: to_secs(m.accessed()),
            ctime_secs: {
                #[cfg(unix)] { m.ctime() as i64 }
                #[cfg(not(unix))] { to_secs(m.created()) }
            },
        }
    }

    fn map_io(e: std::io::Error) -> VfsError {
        match e.kind() {
            std::io::ErrorKind::NotFound => VfsError::NotFound,
            std::io::ErrorKind::PermissionDenied => VfsError::PermissionDenied,
            std::io::ErrorKind::AlreadyExists => VfsError::AlreadyExists,
            _ => VfsError::Io(e.to_string()),
        }
    }

    fn check_rw(&self) -> VfsResult<()> {
        if self.read_only { Err(VfsError::PermissionDenied) } else { Ok(()) }
    }
}

#[async_trait]
impl TokimoVfs for HostFs {
    async fn stat(&self, path: &Path) -> VfsResult<FileAttr> {
        let p = self.resolve(path)?;
        let m = tokio::fs::symlink_metadata(&p).await.map_err(Self::map_io)?;
        Ok(Self::attr_from_meta(&m))
    }
    async fn list(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        let p = self.resolve(path)?;
        let mut rd = tokio::fs::read_dir(&p).await.map_err(Self::map_io)?;
        let mut out = Vec::new();
        while let Some(e) = rd.next_entry().await.map_err(Self::map_io)? {
            let ft = e.file_type().await.map_err(Self::map_io)?;
            let kind = if ft.is_dir() { FsKind::Dir }
                else if ft.is_symlink() { FsKind::Symlink }
                else { FsKind::File };
            out.push(DirEntry { name: e.file_name().to_string_lossy().into_owned(), kind });
        }
        Ok(out)
    }
    async fn read(&self, path: &Path, offset: u64, len: u32) -> VfsResult<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let p = self.resolve(path)?;
        let mut f = tokio::fs::File::open(&p).await.map_err(Self::map_io)?;
        f.seek(std::io::SeekFrom::Start(offset)).await.map_err(Self::map_io)?;
        let mut buf = vec![0u8; len as usize];
        let mut filled = 0;
        while filled < buf.len() {
            let n = f.read(&mut buf[filled..]).await.map_err(Self::map_io)?;
            if n == 0 { break; }
            filled += n;
        }
        buf.truncate(filled);
        Ok(buf)
    }
    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        use tokio::io::{AsyncSeekExt, AsyncWriteExt};
        self.check_rw()?;
        let p = self.resolve(path)?;
        let mut f = tokio::fs::OpenOptions::new().write(true).create(true).open(&p)
            .await.map_err(Self::map_io)?;
        f.seek(std::io::SeekFrom::Start(offset)).await.map_err(Self::map_io)?;
        f.write_all(data).await.map_err(Self::map_io)?;
        Ok(data.len() as u32)
    }
    async fn create(&self, path: &Path, _mode: u32) -> VfsResult<FileAttr> {
        self.check_rw()?;
        let p = self.resolve(path)?;
        let _ = tokio::fs::File::create(&p).await.map_err(Self::map_io)?;
        let m = tokio::fs::symlink_metadata(&p).await.map_err(Self::map_io)?;
        Ok(Self::attr_from_meta(&m))
    }
    async fn mkdir(&self, path: &Path, _mode: u32) -> VfsResult<FileAttr> {
        self.check_rw()?;
        let p = self.resolve(path)?;
        tokio::fs::create_dir(&p).await.map_err(Self::map_io)?;
        let m = tokio::fs::symlink_metadata(&p).await.map_err(Self::map_io)?;
        Ok(Self::attr_from_meta(&m))
    }
    async fn remove(&self, path: &Path) -> VfsResult<()> {
        self.check_rw()?;
        let p = self.resolve(path)?;
        tokio::fs::remove_file(&p).await.map_err(Self::map_io)
    }
    async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        self.check_rw()?;
        let p = self.resolve(path)?;
        tokio::fs::remove_dir(&p).await.map_err(Self::map_io)
    }
    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        self.check_rw()?;
        let f = self.resolve(from)?;
        let t = self.resolve(to)?;
        tokio::fs::rename(&f, &t).await.map_err(Self::map_io)
    }
    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        self.check_rw()?;
        let p = self.resolve(path)?;
        let f = tokio::fs::OpenOptions::new().write(true).open(&p)
            .await.map_err(Self::map_io)?;
        f.set_len(size).await.map_err(Self::map_io)
    }
}

// ---------------------------------------------------------------- errors

#[derive(Error, Debug)]
pub enum Error {
    #[error("sandbox not started")]
    NotStarted,
    #[error("sandbox already started")]
    AlreadyStarted,
    #[error("unsupported on this backend: {0}")]
    Unsupported(&'static str),
    #[error("invalid config: {0}")]
    Config(String),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("hypervisor: {0}")]
    Hypervisor(String),
    #[error("vfs: {0}")]
    Vfs(#[from] VfsError),
    #[error("other: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------- ids

macro_rules! newtype_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub Uuid);
        impl $name { pub fn new() -> Self { Self(Uuid::new_v4()) } }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.0) }
        }
    };
}
newtype_id!(SandboxId);
newtype_id!(MountId);
newtype_id!(ExecId);

// ---------------------------------------------------------------- network

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol { Tcp, Udp }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortForward {
    pub host_port: u16,
    pub guest_port: u16,
    pub protocol: Protocol,
}

#[derive(Debug, Clone)]
pub struct NetworkSpec {
    pub user_mode: bool,
    pub port_forwards: Vec<PortForward>,
    pub dns: Option<Vec<IpAddr>>,
}

impl Default for NetworkSpec {
    /// User-mode NAT only, no host port forwards.
    ///
    /// The core transport between host and guest is a virtio-serial
    /// channel backed by a Unix socket (or named pipe on Windows), so
    /// the guest is functional without opening any host TCP ports.
    fn default() -> Self {
        Self {
            user_mode: true,
            port_forwards: vec![],
            dns: None,
        }
    }
}

// ---------------------------------------------------------------- image

/// Paths to the guest image artifacts.
#[derive(Debug, Clone)]
pub struct ImagePaths {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    /// Optional squashfs rootfs. If None, the initrd is treated as the
    /// full root (busybox-style).
    pub rootfs: Option<PathBuf>,
}

impl ImagePaths {
    /// Resolve from `TOKIMO_IMG_DIR`, then `$CWD/img`, then `$CWD/assets`.
    pub fn from_env_or_default() -> Option<Self> {
        let candidates: Vec<PathBuf> = [
            std::env::var_os("TOKIMO_IMG_DIR").map(PathBuf::from),
            std::env::current_dir().ok().map(|p| p.join("img")),
            std::env::current_dir().ok().map(|p| p.join("assets")),
        ].into_iter().flatten().collect();
        for dir in candidates {
            let kernel = dir.join("vmlinuz");
            let initrd1 = dir.join("initrd.img");
            let initrd2 = dir.join("initramfs.img");
            let initrd = if initrd1.exists() { initrd1 } else { initrd2 };
            if kernel.exists() && initrd.exists() {
                let rootfs = dir.join("rootfs.squashfs");
                return Some(Self {
                    kernel, initrd,
                    rootfs: rootfs.exists().then_some(rootfs),
                });
            }
        }
        None
    }
}

// ---------------------------------------------------------------- config

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub name: String,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub runtime_dir: Option<PathBuf>,
    pub image: Option<ImagePaths>,
    pub extra_cmdline: Vec<String>,
    pub mounts: Vec<MountSpec>,
    pub network: NetworkSpec,
    /// When true, the guest serial console is exposed as a bidirectional
    /// Unix socket instead of a write-only log file, and the guest init
    /// will spawn an interactive `/bin/bash` on the console after running
    /// any `tokimo.script=…` argument. Designed for local debug / REPL.
    pub interactive_serial: bool,
}

impl SandboxConfig {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            vcpus: 1,
            memory_mib: 256,
            runtime_dir: None,
            image: None,
            extra_cmdline: Vec::new(),
            mounts: Vec::new(),
            network: NetworkSpec::default(),
            interactive_serial: false,
        }
    }
    pub fn vcpus(mut self, n: u32) -> Self { self.vcpus = n; self }
    pub fn memory_mib(mut self, m: u32) -> Self { self.memory_mib = m; self }
    pub fn mount(mut self, m: MountSpec) -> Self { self.mounts.push(m); self }
    pub fn image(mut self, i: ImagePaths) -> Self { self.image = Some(i); self }
    pub fn runtime_dir(mut self, p: impl AsRef<Path>) -> Self { self.runtime_dir = Some(p.as_ref().into()); self }
    pub fn network(mut self, n: NetworkSpec) -> Self { self.network = n; self }
    pub fn interactive_serial(mut self, v: bool) -> Self { self.interactive_serial = v; self }
}

// ---------------------------------------------------------------- mounts

#[derive(Clone)]
pub enum MountSpec {
    HostDir {
        guest_path: String,
        host_path: PathBuf,
        read_only: bool,
        tag: String,
    },
    Vfs {
        guest_path: String,
        vfs: Arc<dyn TokimoVfs>,
        read_only: bool,
        tag: String,
    },
}

impl std::fmt::Debug for MountSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountSpec::HostDir { tag, host_path, guest_path, read_only } => f
                .debug_struct("HostDir")
                .field("tag", tag).field("host_path", host_path)
                .field("guest_path", guest_path).field("read_only", read_only).finish(),
            MountSpec::Vfs { tag, guest_path, read_only, .. } => f
                .debug_struct("Vfs").field("tag", tag).field("guest_path", guest_path)
                .field("read_only", read_only).finish_non_exhaustive(),
        }
    }
}

impl MountSpec {
    pub fn tag(&self) -> &str {
        match self { Self::HostDir { tag, .. } | Self::Vfs { tag, .. } => tag }
    }
    pub fn guest_path(&self) -> &str {
        match self { Self::HostDir { guest_path, .. } | Self::Vfs { guest_path, .. } => guest_path }
    }
    pub fn read_only(&self) -> bool {
        match self { Self::HostDir { read_only, .. } | Self::Vfs { read_only, .. } => *read_only }
    }
}

// ---------------------------------------------------------------- exec

#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

impl ExecSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self { program: program.into(), args: vec![], env: vec![], cwd: None }
    }
    pub fn arg(mut self, a: impl Into<String>) -> Self { self.args.push(a.into()); self }
}

#[derive(Debug)]
pub struct ExecOutput {
    pub id: ExecId,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

// ---------------------------------------------------------------- state

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxState { Created, Starting, Running, Stopping, Stopped, Failed }

// ---------------------------------------------------------------- trait

#[async_trait]
pub trait Sandbox: Send + Sync {
    fn id(&self) -> SandboxId;
    fn state(&self) -> SandboxState;
    fn config(&self) -> &SandboxConfig;

    async fn start(&mut self) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;

    async fn add_mount(&mut self, spec: MountSpec) -> Result<MountId>;
    async fn remove_mount(&mut self, id: MountId) -> Result<()>;
    async fn exec(&self, spec: ExecSpec) -> Result<ExecOutput>;
    async fn wait(&mut self) -> Result<()>;

    fn serial_log_path(&self) -> Option<PathBuf> { None }
    /// Bidirectional Unix-socket path for the serial console, when
    /// started with `interactive_serial(true)`. Used for REPL / debug.
    fn serial_socket_path(&self) -> Option<PathBuf> { None }
}
