//! Linux backend: QEMU + KVM.
//!
//! - HostDir mounts are exposed through `virtio-9p-pci`.
//! - VFS mounts are served over a per-mount Unix socket attached to a
//!   shared `virtio-serial-pci` bus. The guest's `tokimo-agent` opens
//!   `/dev/virtio-ports/tokimo.rpc.<tag>` and drives the RPC protocol.
//!
//! No host TCP ports are opened by default: the transport lives entirely
//! in user-owned Unix sockets under the runtime directory.

use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::process::Child;
use tokio::task::JoinHandle;

use tokimo_packages_vm_core::{
    Error, ExecOutput, ExecSpec, HostFs, ImagePaths, MountId, MountSpec, Result, Sandbox,
    SandboxConfig, SandboxId, SandboxState, TokimoVfs,
};
use tokimo_packages_vm_rpc as rpc;

mod qemu;
mod qmp;
mod runtime;

pub use runtime::default_runtime_root;

struct VfsServer {
    tag: String,
    guest_path: String,
    socket_path: PathBuf,
    task: JoinHandle<()>,
}

pub struct QemuSandbox {
    id: SandboxId,
    config: SandboxConfig,
    state: SandboxState,
    runtime: Option<runtime::Runtime>,
    qemu_child: Option<Child>,
    qmp: Option<qmp::QmpClient>,
    vfs_servers: Mutex<Vec<VfsServer>>,
    mount_ids: Mutex<HashMap<MountId, String>>,
    qemu_bin: PathBuf,
}

impl QemuSandbox {
    pub fn new(config: SandboxConfig) -> Self {
        Self {
            id: SandboxId::new(),
            config,
            state: SandboxState::Created,
            runtime: None,
            qemu_child: None,
            qmp: None,
            vfs_servers: Mutex::new(Vec::new()),
            mount_ids: Mutex::new(HashMap::new()),
            qemu_bin: PathBuf::from(qemu::QEMU_DEFAULT),
        }
    }

    pub fn qemu_bin(mut self, p: impl Into<PathBuf>) -> Self {
        self.qemu_bin = p.into();
        self
    }

    fn ensure_runtime(&mut self) -> Result<&runtime::Runtime> {
        if self.runtime.is_none() {
            let root = self
                .config
                .runtime_dir
                .clone()
                .unwrap_or_else(|| runtime::default_runtime_root(&self.config.name));
            self.runtime = Some(runtime::Runtime::new(root)?);
        }
        Ok(self.runtime.as_ref().unwrap())
    }
}

/// Build the kernel command line.
///
/// Mounts are encoded as `tokimo.mounts=kind:tag:path,...`. `kind` is
/// either `9p` (HostDir) or `vfs` (virtio-serial RPC).
fn build_cmdline(extra: &[String], mounts: &[(String, String)], has_rootfs: bool) -> String {
    let mut parts: Vec<String> = vec![
        "console=ttyS0".into(),
        "panic=-1".into(),
        "quiet".into(),
        "loglevel=3".into(),
    ];
    if has_rootfs {
        parts.push("root=/dev/vda".into());
        parts.push("rootfstype=squashfs".into());
        parts.push("ro".into());
    }
    if !mounts.is_empty() {
        // Format: `tag:guest_path` (comma-separated). All mounts use the
        // same virtio-serial/FUSE transport in the guest.
        let s = mounts
            .iter()
            .map(|(tag, gp)| format!("{tag}:{gp}"))
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("tokimo.mounts={}", s));
    }
    // tokimo.script=... must come LAST so the guest parser can read
    // everything after the `=` to end-of-line.
    let (script, rest): (Vec<&String>, Vec<&String>) =
        extra.iter().partition(|s| s.starts_with("tokimo.script="));
    parts.extend(rest.into_iter().cloned());
    parts.extend(script.into_iter().cloned());
    parts.join(" ")
}

#[async_trait]
impl Sandbox for QemuSandbox {
    fn id(&self) -> SandboxId {
        self.id
    }
    fn state(&self) -> SandboxState {
        self.state
    }
    fn config(&self) -> &SandboxConfig {
        &self.config
    }

    async fn start(&mut self) -> Result<()> {
        if self.state != SandboxState::Created {
            return Err(Error::AlreadyStarted);
        }
        self.state = SandboxState::Starting;

        self.ensure_runtime()?;

        let image = self
            .config
            .image
            .clone()
            .or_else(ImagePaths::from_env_or_default)
            .ok_or_else(|| {
                Error::Config(
                    "no image configured; run scripts/image/build.sh or set TOKIMO_IMG_DIR".into(),
                )
            })?;

        let specs: Vec<MountSpec> = self.config.mounts.clone();
        let mut serial_ports: Vec<qemu::SerialPort> = Vec::new();
        // (tag, guest_path)
        let mut cmdline_mounts: Vec<(String, String)> = Vec::new();
        let mut ids_out: HashMap<MountId, String> = HashMap::new();

        let runtime_root = self.runtime.as_ref().unwrap().root.clone();

        for (i, m) in specs.iter().enumerate() {
            let tag = m.tag().to_string();
            let gp = m.guest_path().to_string();
            let id = MountId::new();
            ids_out.insert(id, tag.clone());

            // Every mount — whether HostDir or Vfs — is served over a
            // per-mount virtio-serial Unix socket. HostDir becomes an
            // internal `HostFs` adapter so there is a single code path
            // in the guest (FUSE proxy), no 9p/virtiofs dependency.
            let vfs: Arc<dyn TokimoVfs> = match m {
                MountSpec::HostDir {
                    host_path,
                    read_only,
                    ..
                } => Arc::new(HostFs::new(host_path.clone(), *read_only)),
                MountSpec::Vfs { vfs, .. } => vfs.clone(),
            };
            let sock_path = runtime_root.join(format!("rpc{i}.sock"));
            let _ = std::fs::remove_file(&sock_path);
            let listener = UnixListener::bind(&sock_path)
                .map_err(|e| Error::Hypervisor(format!("rpc bind {}: {e}", sock_path.display())))?;
            let sp = sock_path.clone();
            let task = tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((sock, _)) => {
                            let vfs = vfs.clone();
                            tokio::spawn(async move {
                                let _ = rpc::serve_stream(sock, vfs).await;
                            });
                        }
                        Err(e) => {
                            tracing::debug!("rpc listener {:?}: {e}", sp);
                            break;
                        }
                    }
                }
            });
            self.vfs_servers.lock().push(VfsServer {
                tag: tag.clone(),
                guest_path: gp.clone(),
                socket_path: sock_path.clone(),
                task,
            });
            serial_ports.push(qemu::SerialPort {
                chardev_id: format!("rpc{i}"),
                port_name: format!("tokimo.rpc.{tag}"),
                socket_path: sock_path,
            });
            cmdline_mounts.push((tag, gp));
        }

        let cmdline = build_cmdline(
            &self.config.extra_cmdline,
            &cmdline_mounts,
            image.rootfs.is_some(),
        );
        let cmdline = if self.config.interactive_serial {
            format!("{cmdline} tokimo.shell=1")
        } else {
            cmdline
        };
        tracing::debug!("kernel cmdline: {cmdline}");
        let rt = self.runtime.as_ref().unwrap();
        let serial_socket = if self.config.interactive_serial {
            Some(rt.serial_sock.as_path())
        } else {
            None
        };
        let spec = qemu::QemuSpec {
            qemu_bin: &self.qemu_bin,
            vcpus: self.config.vcpus,
            memory_mib: self.config.memory_mib,
            kernel: &image.kernel,
            initrd: &image.initrd,
            rootfs: image.rootfs.as_deref(),
            cmdline: &cmdline,
            serial_ports: &serial_ports,
            network: &self.config.network,
            qmp_socket: &rt.qmp_sock,
            serial_log: &rt.serial_log,
            serial_socket,
            monitor_socket: &rt.monitor_sock,
            runtime_dir: &rt.root,
        };
        let cmd = qemu::build_command(&spec);
        let child = qemu::spawn(cmd)
            .await
            .map_err(|e| Error::Hypervisor(format!("qemu spawn: {e}")))?;
        self.qemu_child = Some(child);

        let qmp = qmp::QmpClient::connect(&rt.qmp_sock)
            .await
            .map_err(|e| Error::Hypervisor(format!("qmp connect: {e}")))?;
        self.qmp = Some(qmp);

        *self.mount_ids.lock() = ids_out;
        self.state = SandboxState::Running;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if self.state != SandboxState::Running && self.state != SandboxState::Starting {
            return Ok(());
        }
        self.state = SandboxState::Stopping;
        if let Some(mut qmp) = self.qmp.take() {
            let _ = qmp.powerdown().await;
            let _ = tokio::time::timeout(Duration::from_secs(3), qmp.quit()).await;
        }
        if let Some(mut child) = self.qemu_child.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        let servers = std::mem::take(&mut *self.vfs_servers.lock());
        for s in servers {
            s.task.abort();
            let _ = s.task.await;
            let _ = std::fs::remove_file(&s.socket_path);
            tracing::debug!(
                "cleaned rpc socket {:?} (tag={} -> {})",
                s.socket_path,
                s.tag,
                s.guest_path
            );
        }
        self.state = SandboxState::Stopped;
        Ok(())
    }

    async fn add_mount(&mut self, _s: MountSpec) -> Result<MountId> {
        Err(Error::Unsupported("hot add_mount not implemented"))
    }
    async fn remove_mount(&mut self, _id: MountId) -> Result<()> {
        Err(Error::Unsupported("hot remove_mount not implemented"))
    }
    async fn exec(&self, _s: ExecSpec) -> Result<ExecOutput> {
        Err(Error::Unsupported("guest exec not wired in this build"))
    }
    async fn wait(&mut self) -> Result<()> {
        if let Some(child) = self.qemu_child.as_mut() {
            let _ = child.wait().await;
            self.state = SandboxState::Stopped;
        }
        Ok(())
    }
    fn serial_log_path(&self) -> Option<PathBuf> {
        self.runtime.as_ref().map(|r| r.serial_log.clone())
    }
    fn serial_socket_path(&self) -> Option<PathBuf> {
        if self.config.interactive_serial {
            self.runtime.as_ref().map(|r| r.serial_sock.clone())
        } else {
            None
        }
    }
}

impl Drop for QemuSandbox {
    fn drop(&mut self) {
        if let Some(mut c) = self.qemu_child.take() {
            let _ = c.start_kill();
        }
        for s in self.vfs_servers.lock().drain(..) {
            s.task.abort();
            let _ = std::fs::remove_file(&s.socket_path);
        }
    }
}

#[allow(dead_code)]
fn _require_arc_sync(_: Arc<dyn tokimo_packages_vm_core::TokimoVfs>) {}
