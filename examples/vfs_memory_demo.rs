//! Example: serve an in-memory TokimoVfs to the guest via RPC.
//!
//! Guest's tokimo-agent mounts this as a FUSE filesystem at `/mnt/mem`.
//! Guest init then `cat`s `/mnt/mem/hello.txt` into the serial log.
//!
//! Run: `cargo run --example vfs_memory_demo`

use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokimo_packages_vm::{
    DirEntry, FileAttr, FsKind, MountSpec, SandboxConfig, TokimoVfs, VfsError, VfsResult,
};

const MAGIC: &str = "hello-from-tokimo-vfs";

// ---------------- simple in-memory filesystem ----------------

enum Node {
    File(Vec<u8>),
    Dir,
}

struct MemoryVfs {
    inner: Mutex<HashMap<PathBuf, Node>>,
}

impl MemoryVfs {
    fn new() -> Self {
        let mut m = HashMap::new();
        m.insert(PathBuf::from("/"), Node::Dir);
        m.insert(
            PathBuf::from("/hello.txt"),
            Node::File(format!("{MAGIC}\n").into_bytes()),
        );
        Self {
            inner: Mutex::new(m),
        }
    }
    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
    fn attr_of(n: &Node) -> FileAttr {
        let (kind, size) = match n {
            Node::File(d) => (FsKind::File, d.len() as u64),
            Node::Dir => (FsKind::Dir, 0),
        };
        let t = Self::now_secs();
        FileAttr {
            size,
            mode: if matches!(kind, FsKind::Dir) {
                0o755
            } else {
                0o644
            },
            kind,
            mtime_secs: t,
            atime_secs: t,
            ctime_secs: t,
        }
    }
}

#[async_trait]
impl TokimoVfs for MemoryVfs {
    async fn stat(&self, path: &Path) -> VfsResult<FileAttr> {
        let g = self.inner.lock().unwrap();
        g.get(path).map(Self::attr_of).ok_or(VfsError::NotFound)
    }
    async fn list(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        let g = self.inner.lock().unwrap();
        match g.get(path) {
            Some(Node::Dir) => {
                let mut out = vec![];
                for (p, n) in g.iter() {
                    let p: &PathBuf = p;
                    if p == path {
                        continue;
                    }
                    if p.parent() == Some(path) {
                        let name = p
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("")
                            .to_string();
                        let kind = match n {
                            Node::File(_) => FsKind::File,
                            Node::Dir => FsKind::Dir,
                        };
                        out.push(DirEntry { name, kind });
                    }
                }
                Ok(out)
            }
            Some(_) => Err(VfsError::NotADirectory),
            None => Err(VfsError::NotFound),
        }
    }
    async fn read(&self, path: &Path, offset: u64, len: u32) -> VfsResult<Vec<u8>> {
        let g = self.inner.lock().unwrap();
        match g.get(path) {
            Some(Node::File(d)) => {
                let o = offset as usize;
                let end = (o + len as usize).min(d.len());
                Ok(if o >= d.len() {
                    vec![]
                } else {
                    d[o..end].to_vec()
                })
            }
            Some(_) => Err(VfsError::IsADirectory),
            None => Err(VfsError::NotFound),
        }
    }
    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        let mut g = self.inner.lock().unwrap();
        match g.get_mut(path) {
            Some(Node::File(d)) => {
                let o = offset as usize;
                if d.len() < o + data.len() {
                    d.resize(o + data.len(), 0);
                }
                d[o..o + data.len()].copy_from_slice(data);
                Ok(data.len() as u32)
            }
            Some(_) => Err(VfsError::IsADirectory),
            None => Err(VfsError::NotFound),
        }
    }
    async fn create(&self, path: &Path, _mode: u32) -> VfsResult<FileAttr> {
        let mut g = self.inner.lock().unwrap();
        if g.contains_key(path) {
            return Err(VfsError::AlreadyExists);
        }
        g.insert(path.to_path_buf(), Node::File(Vec::new()));
        Ok(Self::attr_of(g.get(path).unwrap()))
    }
    async fn mkdir(&self, path: &Path, _mode: u32) -> VfsResult<FileAttr> {
        let mut g = self.inner.lock().unwrap();
        if g.contains_key(path) {
            return Err(VfsError::AlreadyExists);
        }
        g.insert(path.to_path_buf(), Node::Dir);
        Ok(Self::attr_of(g.get(path).unwrap()))
    }
    async fn remove(&self, path: &Path) -> VfsResult<()> {
        let mut g = self.inner.lock().unwrap();
        match g.remove(path) {
            Some(_) => Ok(()),
            None => Err(VfsError::NotFound),
        }
    }
    async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        let mut g = self.inner.lock().unwrap();
        match g.remove(path) {
            Some(_) => Ok(()),
            None => Err(VfsError::NotFound),
        }
    }
    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        let mut g = self.inner.lock().unwrap();
        let node = g.remove(from).ok_or(VfsError::NotFound)?;
        g.insert(to.to_path_buf(), node);
        Ok(())
    }
    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        let mut g = self.inner.lock().unwrap();
        match g.get_mut(path) {
            Some(Node::File(d)) => {
                d.resize(size as usize, 0);
                Ok(())
            }
            Some(_) => Err(VfsError::IsADirectory),
            None => Err(VfsError::NotFound),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("tokimo=info".parse()?),
        )
        .init();

    let vfs: Arc<dyn TokimoVfs> = Arc::new(MemoryVfs::new());

    let mut cfg = SandboxConfig::new("vfs-memory-demo")
        .vcpus(1)
        .memory_mib(256)
        .mount(MountSpec::Vfs {
            tag: "mem".into(),
            vfs,
            guest_path: "/mnt/mem".into(),
            read_only: false,
        });
    cfg.extra_cmdline.push(
        "tokimo.script=python3 -c 'print(open(\"/mnt/mem/hello.txt\").read().strip())'".into(),
    );

    let mut sbx = tokimo_packages_vm::new_default(cfg);
    println!(">>> starting sandbox");
    sbx.start().await?;
    let log = sbx.serial_log_path().expect("serial log");
    println!(">>> serial log: {}", log.display());

    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut found = false;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(s) = std::fs::read_to_string(&log) {
            if s.contains(MAGIC) {
                println!("\n===== serial output =====\n{s}\n========================");
                found = true;
                break;
            }
        }
    }
    sbx.stop().await?;
    if !found {
        anyhow::bail!("did not observe magic string");
    }
    println!(">>> success");
    Ok(())
}
