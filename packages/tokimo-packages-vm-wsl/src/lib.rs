//! Windows Subsystem for Linux (WSL2) backend for tokimo sandboxes.
//!
//! Rationale: on Windows we refuse to require the user to install QEMU.
//! Modern Windows already ships with a fully-functional hypervisor via
//! WSL2, so we use it. Each sandbox becomes an isolated WSL distro
//! imported from the same Debian tarball produced by
//! `scripts/image/build.sh`; our VFS RPC runs over `wsl.exe` stdio, so
//! no TCP ports are opened on the host and no FUSE/WinFsp is required
//! host-side.
//!
//! ```text
//! Host (Windows)                        WSL2 distro (Debian)
//! ┌───────────────┐    stdin/stdout    ┌─────────────────────┐
//! │ Arc<TokimoVfs>│ ──────────────────▶│ tokimo-agent --stdio│
//! │  serve_stream │ ◀──────────────────│   │                 │
//! └───────────────┘                    │   └─▶ FUSE /mnt/<t> │
//!                                      └─────────────────────┘
//!     spawn: wsl.exe -d tokimo-<id> --exec /sbin/tokimo-agent --stdio …
//! ```
//!
//! Only `wsl.exe` is required on the host. On Windows 10 21H2+ and all
//! Windows 11, WSL2 is a default platform component and can be enabled
//! with `wsl --install` (no admin after initial, no reboot on recent
//! builds).

use async_trait::async_trait;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use tokimo_packages_vm_core::{
    Error, ExecOutput, ExecSpec, HostFs, MountId, MountSpec, Result, Sandbox, SandboxConfig,
    SandboxId, SandboxState, TokimoVfs,
};
use tokimo_packages_vm_rpc as rpc;

// ---------------------------------------------------------------- config

/// Extra knobs specific to the WSL backend. Most callers won't need to
/// touch these — sensible defaults are applied in [`WslSandbox::new`].
#[derive(Debug, Clone)]
pub struct WslConfig {
    /// Distro name inside WSL. Must be unique per running sandbox.
    /// Defaults to `tokimo-<sandbox-id>`.
    pub distro_name: Option<String>,
    /// Directory where WSL stores this distro's virtual hard disk.
    /// Defaults to `%LOCALAPPDATA%\tokimo\sandboxes\<distro_name>\`.
    pub install_dir: Option<PathBuf>,
    /// Path to the Debian rootfs tarball to import. Defaults to
    /// `<TOKIMO_IMG_DIR>/rootfs.tar` (produced by
    /// `scripts/image/build.sh`).
    pub rootfs_tar: Option<PathBuf>,
    /// Path to the `wsl.exe` binary. Defaults to `"wsl.exe"` on PATH.
    pub wsl_bin: PathBuf,
}

impl Default for WslConfig {
    fn default() -> Self {
        Self {
            distro_name: None,
            install_dir: None,
            rootfs_tar: None,
            wsl_bin: PathBuf::from("wsl.exe"),
        }
    }
}

// ---------------------------------------------------------------- sandbox

pub struct WslSandbox {
    id: SandboxId,
    state: SandboxState,
    config: SandboxConfig,
    wsl: WslConfig,
    resolved_distro: Option<String>,
    agent_children: Arc<Mutex<Vec<Child>>>,
    vfs_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    runtime_dir: Option<PathBuf>,
}

impl WslSandbox {
    pub fn new(config: SandboxConfig) -> Self {
        Self::with_wsl_config(config, WslConfig::default())
    }

    pub fn with_wsl_config(config: SandboxConfig, wsl: WslConfig) -> Self {
        Self {
            id: SandboxId::new(),
            state: SandboxState::Created,
            config,
            wsl,
            resolved_distro: None,
            agent_children: Arc::new(Mutex::new(Vec::new())),
            vfs_tasks: Arc::new(Mutex::new(Vec::new())),
            runtime_dir: None,
        }
    }

    fn resolve_distro_name(&self) -> String {
        self.wsl.distro_name.clone().unwrap_or_else(|| {
            // Keep names short so `wsl -d <name>` commands stay readable.
            let id = format!("{}", self.id.0.simple());
            let short = &id[..8.min(id.len())];
            format!("tokimo-{}-{}", sanitize(&self.config.name), short)
        })
    }

    async fn ensure_distro(&mut self) -> Result<String> {
        let name = self.resolve_distro_name();
        // `wsl -l -q` lists existing distros; names are UTF-16 on stdout.
        let out = Command::new(&self.wsl.wsl_bin)
            .arg("--list")
            .arg("--quiet")
            .output()
            .await
            .map_err(|e| Error::Hypervisor(format!("wsl --list: {e}")))?;
        let listing = decode_wsl_text(&out.stdout);
        if listing.lines().any(|l| l.trim() == name) {
            self.resolved_distro = Some(name.clone());
            return Ok(name);
        }

        // Not imported yet — do the one-time import.
        let install_dir = self
            .wsl
            .install_dir
            .clone()
            .unwrap_or_else(|| default_install_dir(&name));
        std::fs::create_dir_all(&install_dir)
            .map_err(|e| Error::Config(format!("create {install_dir:?}: {e}")))?;
        let tar = self
            .wsl
            .rootfs_tar
            .clone()
            .or_else(|| {
                std::env::var_os("TOKIMO_IMG_DIR").map(|d| PathBuf::from(d).join("rootfs.tar"))
            })
            .unwrap_or_else(|| PathBuf::from("img/rootfs.tar"));
        if !tar.exists() {
            return Err(Error::Config(format!(
                "rootfs tarball not found at {tar:?} — run scripts/image/build.sh (WSL backend expects a .tar, not .squashfs)"
            )));
        }
        tracing::info!("wsl --import {} {:?} {:?}", name, install_dir, tar);
        let st = Command::new(&self.wsl.wsl_bin)
            .arg("--import")
            .arg(&name)
            .arg(&install_dir)
            .arg(&tar)
            .arg("--version")
            .arg("2")
            .status()
            .await
            .map_err(|e| Error::Hypervisor(format!("wsl --import: {e}")))?;
        if !st.success() {
            return Err(Error::Hypervisor(format!("wsl --import exited {st}")));
        }
        self.resolved_distro = Some(name.clone());
        Ok(name)
    }

    fn default_runtime_dir(&self) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tokimo-{}-{}",
            self.config.name,
            std::process::id()
        ))
    }
}

#[async_trait]
impl Sandbox for WslSandbox {
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

        let rt_dir = self
            .config
            .runtime_dir
            .clone()
            .unwrap_or_else(|| self.default_runtime_dir());
        std::fs::create_dir_all(&rt_dir).map_err(|e| Error::Config(format!("runtime_dir: {e}")))?;
        self.runtime_dir = Some(rt_dir.clone());

        let distro = self.ensure_distro().await?;

        // Per-mount agent + RPC task over stdio.
        for (i, m) in self.config.mounts.clone().iter().enumerate() {
            let tag = m.tag().to_string();
            let guest_path = m.guest_path().to_string();
            let vfs: Arc<dyn TokimoVfs> = match m {
                MountSpec::HostDir {
                    host_path,
                    read_only,
                    ..
                } => Arc::new(HostFs::new(host_path.clone(), *read_only)),
                MountSpec::Vfs { vfs, .. } => vfs.clone(),
            };

            let log_path = rt_dir.join(format!("agent-{tag}.log"));
            let log = std::fs::File::create(&log_path)
                .map_err(|e| Error::Config(format!("agent log: {e}")))?;

            let mut cmd = Command::new(&self.wsl.wsl_bin);
            cmd.arg("-d")
                .arg(&distro)
                .arg("--exec")
                .arg("/sbin/tokimo-agent")
                .arg("--stdio")
                .arg("--mount")
                .arg(&guest_path)
                .arg("--tag")
                .arg(&tag)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::from(log));
            let mut child = cmd
                .spawn()
                .map_err(|e| Error::Hypervisor(format!("spawn wsl agent #{i}: {e}")))?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| Error::Hypervisor("child stdin".into()))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| Error::Hypervisor("child stdout".into()))?;
            self.agent_children.lock().push(child);

            let duplex = ChildDuplex {
                r: stdout,
                w: stdin,
            };
            let vfs_clone = vfs.clone();
            let handle = tokio::spawn(async move {
                if let Err(e) = rpc::serve_stream(duplex, vfs_clone).await {
                    tracing::warn!("wsl rpc serve_stream exited: {e:#}");
                }
            });
            self.vfs_tasks.lock().push(handle);
        }

        self.state = SandboxState::Running;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if self.state == SandboxState::Stopped {
            return Ok(());
        }
        self.state = SandboxState::Stopping;

        // Close stdio to agents (triggers EOF) and reap.
        let children: Vec<_> = self.agent_children.lock().drain(..).collect();
        for mut c in children {
            let _ = c.kill().await;
        }
        let tasks: Vec<_> = self.vfs_tasks.lock().drain(..).collect();
        for t in tasks {
            t.abort();
        }
        // Terminate the distro so next start is clean.
        if let Some(distro) = self.resolved_distro.clone() {
            let _ = Command::new(&self.wsl.wsl_bin)
                .arg("--terminate")
                .arg(&distro)
                .status()
                .await;
        }
        self.state = SandboxState::Stopped;
        Ok(())
    }

    async fn add_mount(&mut self, _s: MountSpec) -> Result<MountId> {
        Err(Error::Unsupported(
            "hot add_mount not implemented on WSL backend",
        ))
    }
    async fn remove_mount(&mut self, _id: MountId) -> Result<()> {
        Err(Error::Unsupported(
            "hot remove_mount not implemented on WSL backend",
        ))
    }

    async fn exec(&self, spec: ExecSpec) -> Result<ExecOutput> {
        let distro = self
            .resolved_distro
            .clone()
            .ok_or_else(|| Error::Hypervisor("sandbox not started".into()))?;
        let mut cmd = Command::new(&self.wsl.wsl_bin);
        cmd.arg("-d").arg(&distro).arg("--exec").arg(&spec.program);
        for a in &spec.args {
            cmd.arg(a);
        }
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let out = cmd
            .output()
            .await
            .map_err(|e| Error::Hypervisor(format!("wsl exec: {e}")))?;
        Ok(ExecOutput {
            id: tokimo_packages_vm_core::ExecId::new(),
            exit_code: out.status.code(),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    async fn wait(&mut self) -> Result<()> {
        // WSL distros are long-lived; there's no single "main process"
        // to wait on. Block until every per-mount agent has exited.
        let children: Vec<_> = self.agent_children.lock().drain(..).collect();
        for mut c in children {
            let _ = c.wait().await;
        }
        self.state = SandboxState::Stopped;
        Ok(())
    }

    fn serial_log_path(&self) -> Option<PathBuf> {
        self.runtime_dir.as_ref().map(|d| d.join("serial.log"))
    }
}

// ---------------------------------------------------------------- helpers

/// Join child stdout+stdin into a single duplex stream.
struct ChildDuplex {
    r: ChildStdout,
    w: ChildStdin,
}
impl AsyncRead for ChildDuplex {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.r).poll_read(cx, buf)
    }
}
impl AsyncWrite for ChildDuplex {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        b: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.w).poll_write(cx, b)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.w).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.w).poll_shutdown(cx)
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn decode_wsl_text(bytes: &[u8]) -> String {
    // `wsl --list` outputs UTF-16LE on Windows.
    if bytes.len() >= 2 && bytes.len().is_multiple_of(2) {
        let u16s: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&u16s)
    } else {
        String::from_utf8_lossy(bytes).to_string()
    }
}

fn default_install_dir(distro: &str) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(base) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(base)
                .join("tokimo")
                .join("sandboxes")
                .join(distro);
        }
    }
    std::env::temp_dir()
        .join("tokimo")
        .join("sandboxes")
        .join(distro)
}
