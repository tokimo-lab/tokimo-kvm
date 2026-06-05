//! FUSE filesystem that proxies to the host over a virtio-serial RPC channel.

use clap::Parser;
use fuser::{
    FileAttr as FuseAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokimo_packages_vm_core::{FileAttr, FsKind, VfsError};
use tokimo_packages_vm_rpc::Client;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::runtime::Runtime;

const TTL: Duration = Duration::from_secs(1);

/// Duplex transport that reads from process stdin and writes to stdout.
/// Used by the WSL backend to tunnel RPC through `wsl.exe` stdio.
struct StdioDuplex {
    r: tokio::io::Stdin,
    w: tokio::io::Stdout,
}
impl StdioDuplex {
    fn new() -> Self {
        Self {
            r: tokio::io::stdin(),
            w: tokio::io::stdout(),
        }
    }
}
impl AsyncRead for StdioDuplex {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.r).poll_read(cx, buf)
    }
}
impl AsyncWrite for StdioDuplex {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        b: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.w).poll_write(cx, b)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.w).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.w).poll_shutdown(cx)
    }
}

#[derive(Parser, Debug)]
#[command(name = "tokimo-agent")]
struct Args {
    /// virtio-serial port name exposed by the host. Looked up under
    /// `/dev/virtio-ports/<name>` unless given as an absolute path.
    /// Mutually exclusive with `--stdio`.
    #[arg(long)]
    port_name: Option<String>,
    /// When set, read RPC requests from stdin and write responses to
    /// stdout (used by the WSL backend, where `wsl.exe` forwards our
    /// stdio across the host/distro boundary).
    #[arg(long, default_value_t = false)]
    stdio: bool,
    /// Mount point in the guest (e.g. /mnt/mem).
    #[arg(long)]
    mount: PathBuf,
    /// Mount tag, used as the FUSE fsname.
    #[arg(long, default_value = "tokimo")]
    tag: String,
}

pub fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    eprintln!(
        "tokimo-agent starting: port={:?} stdio={} mount={:?} tag={}",
        args.port_name, args.stdio, args.mount, args.tag
    );

    let rt = Runtime::new()?;
    let client = if args.stdio {
        eprintln!("tokimo-agent: using stdio transport");
        // Join stdin + stdout into a single duplex AsyncRead+AsyncWrite.
        let joined = StdioDuplex::new();
        let c = rt
            .block_on(async { Client::handshake_from_stream(joined).await })
            .map_err(|e| {
                eprintln!("tokimo-agent: stdio handshake failed: {e:#}");
                e
            })?;
        c
    } else {
        let port = args
            .port_name
            .clone()
            .ok_or_else(|| anyhow::anyhow!("either --port-name or --stdio is required"))?;
        eprintln!("tokimo-agent: connecting to virtio-serial port {port}");
        rt.block_on(async { Client::connect_port(&port).await })
            .map_err(|e| {
                eprintln!("tokimo-agent: connect_port failed: {e:#}");
                e
            })?
    };
    eprintln!("tokimo-agent: connected; mounting FUSE at {:?}", args.mount);
    std::fs::create_dir_all(&args.mount).ok();

    let fs = TokimoFuse {
        rt: Arc::new(rt),
        client: Arc::new(client),
        inodes: HashMap::new(),
        rev: HashMap::new(),
        next_ino: 2,
        fhs: HashMap::new(),
        next_fh: 1,
    };
    let options = vec![
        MountOption::FSName(format!("tokimo-{}", args.tag)),
        MountOption::AllowOther,
    ];
    // Foreground mount.
    fuser::mount2(fs, &args.mount, &options)?;
    Ok(())
}

struct TokimoFuse {
    rt: Arc<Runtime>,
    client: Arc<Client>,
    // ino -> path
    inodes: HashMap<u64, PathBuf>,
    rev: HashMap<PathBuf, u64>,
    next_ino: u64,
    // fh -> path (file handles)
    fhs: HashMap<u64, PathBuf>,
    next_fh: u64,
}

impl TokimoFuse {
    fn root_path() -> PathBuf {
        PathBuf::from("/")
    }

    fn path_for(&self, ino: u64) -> Option<PathBuf> {
        if ino == 1 {
            Some(Self::root_path())
        } else {
            self.inodes.get(&ino).cloned()
        }
    }

    fn intern(&mut self, path: PathBuf) -> u64 {
        if path == Path::new("/") {
            return 1;
        }
        if let Some(&ino) = self.rev.get(&path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(ino, path.clone());
        self.rev.insert(path, ino);
        ino
    }

    fn alloc_fh(&mut self, path: PathBuf) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.fhs.insert(fh, path);
        fh
    }

    fn to_fuse_attr(&self, ino: u64, a: &FileAttr) -> FuseAttr {
        let kind = match a.kind {
            FsKind::File => FileType::RegularFile,
            FsKind::Dir => FileType::Directory,
            FsKind::Symlink => FileType::Symlink,
        };
        let t = |s: i64| UNIX_EPOCH + Duration::from_secs(s.max(0) as u64);
        FuseAttr {
            ino,
            size: a.size,
            blocks: a.size.div_ceil(512),
            atime: t(a.atime_secs),
            mtime: t(a.mtime_secs),
            ctime: t(a.ctime_secs),
            crtime: t(a.ctime_secs),
            kind,
            perm: (a.mode & 0o7777) as u16,
            nlink: if matches!(a.kind, FsKind::Dir) { 2 } else { 1 },
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

fn errno(e: &VfsError) -> i32 {
    match e {
        VfsError::NotFound => libc::ENOENT,
        VfsError::PermissionDenied => libc::EACCES,
        VfsError::AlreadyExists => libc::EEXIST,
        VfsError::NotADirectory => libc::ENOTDIR,
        VfsError::IsADirectory => libc::EISDIR,
        VfsError::Io(_) => libc::EIO,
    }
}

fn join(parent: &Path, name: &OsStr) -> PathBuf {
    let mut p = parent.to_path_buf();
    p.push(name);
    p
}

impl Filesystem for TokimoFuse {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let p = match self.path_for(parent) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        let path = join(&p, name);
        let client = self.client.clone();
        let path2 = path.clone();
        let res = self.rt.block_on(async move { client.stat(&path2).await });
        match res {
            Ok(a) => {
                let ino = self.intern(path);
                let attr = self.to_fuse_attr(ino, &a);
                reply.entry(&TTL, &attr, 0);
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let p = match self.path_for(ino) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        let client = self.client.clone();
        let res = self.rt.block_on(async move { client.stat(&p).await });
        match res {
            Ok(a) => {
                let attr = self.to_fuse_attr(ino, &a);
                reply.attr(&TTL, &attr);
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let p = match self.path_for(ino) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        if let Some(sz) = size {
            let client = self.client.clone();
            let p2 = p.clone();
            let r = self
                .rt
                .block_on(async move { client.truncate(&p2, sz).await });
            if let Err(e) = r {
                return reply.error(errno(&e));
            }
        }
        let client = self.client.clone();
        let p2 = p.clone();
        match self.rt.block_on(async move { client.stat(&p2).await }) {
            Ok(a) => {
                let attr = self.to_fuse_attr(ino, &a);
                reply.attr(&TTL, &attr);
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let p = match self.path_for(ino) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        let fh = self.alloc_fh(p);
        reply.opened(fh, 0);
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        self.fhs.remove(&fh);
        reply.ok();
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let p = match self.path_for(ino) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        let client = self.client.clone();
        let p2 = p.clone();
        let entries = match self.rt.block_on(async move { client.list(&p2).await }) {
            Ok(v) => v,
            Err(e) => return reply.error(errno(&e)),
        };
        // Pseudo . and ..
        let mut all: Vec<(FileType, String)> = vec![
            (FileType::Directory, ".".into()),
            (FileType::Directory, "..".into()),
        ];
        for e in entries {
            let kind = match e.kind {
                FsKind::File => FileType::RegularFile,
                FsKind::Dir => FileType::Directory,
                FsKind::Symlink => FileType::Symlink,
            };
            all.push((kind, e.name));
        }
        for (i, (kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            let child_path = if name == "." || name == ".." {
                p.clone()
            } else {
                let mut c = p.clone();
                c.push(&name);
                c
            };
            let ino = if name == "." {
                ino
            } else if name == ".." {
                1
            } else {
                self.intern(child_path)
            };
            if reply.add(ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let p = match self.path_for(ino) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        let fh = self.alloc_fh(p);
        reply.opened(fh, 0);
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.fhs.remove(&fh);
        reply.ok();
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        let p = match self.fhs.get(&fh) {
            Some(p) => p.clone(),
            None => return reply.error(libc::EBADF),
        };
        let client = self.client.clone();
        match self
            .rt
            .block_on(async move { client.read(&p, offset.max(0) as u64, size).await })
        {
            Ok(d) => reply.data(&d),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyWrite,
    ) {
        let p = match self.fhs.get(&fh) {
            Some(p) => p.clone(),
            None => return reply.error(libc::EBADF),
        };
        let data = data.to_vec();
        let client = self.client.clone();
        match self
            .rt
            .block_on(async move { client.write(&p, offset.max(0) as u64, &data).await })
        {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let p = match self.path_for(parent) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        let path = join(&p, name);
        let client = self.client.clone();
        let path2 = path.clone();
        match self
            .rt
            .block_on(async move { client.create(&path2, mode).await })
        {
            Ok(a) => {
                let ino = self.intern(path.clone());
                let attr = self.to_fuse_attr(ino, &a);
                let fh = self.alloc_fh(path);
                reply.created(&TTL, &attr, 0, fh, 0);
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let p = match self.path_for(parent) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        let path = join(&p, name);
        let client = self.client.clone();
        let path2 = path.clone();
        match self
            .rt
            .block_on(async move { client.mkdir(&path2, mode).await })
        {
            Ok(a) => {
                let ino = self.intern(path);
                let attr = self.to_fuse_attr(ino, &a);
                reply.entry(&TTL, &attr, 0);
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let p = match self.path_for(parent) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        let path = join(&p, name);
        let client = self.client.clone();
        match self.rt.block_on(async move { client.remove(&path).await }) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let p = match self.path_for(parent) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        let path = join(&p, name);
        let client = self.client.clone();
        match self.rt.block_on(async move { client.rmdir(&path).await }) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let p = match self.path_for(parent) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        let np = match self.path_for(newparent) {
            Some(p) => p,
            None => return reply.error(libc::ENOENT),
        };
        let from = join(&p, name);
        let to = join(&np, newname);
        let client = self.client.clone();
        match self
            .rt
            .block_on(async move { client.rename(&from, &to).await })
        {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }
}
