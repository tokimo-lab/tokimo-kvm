//! macOS backend: Apple's Virtualization.framework.
//!
//! Design
//! ------
//! * We drive `Virtualization.framework` via [`objc2-virtualization`] bindings.
//! * A [`VZLinuxBootLoader`] boots the shared kernel + initramfs image
//!   produced by `scripts/image/build.sh`.
//! * Each [`MountSpec`] becomes either:
//!   - `HostDir` → `VZVirtioFileSystemDeviceConfiguration` with
//!     `VZSingleDirectoryShare`.
//!   - `Vfs`     → host-side TCP RPC listener; the guest's `tokimo-agent`
//!     connects over `VZNATNetworkDeviceAttachment` and mounts a FUSE
//!     filesystem inside the guest. No host-side FUSE.
//! * A `VZVirtioSocketDeviceConfiguration` is attached so a future
//!   iteration can swap TCP for `VZVirtioSocketListener`.
//! * The `VZVirtualMachine` itself is queue-bound and **not `Send`**, so
//!   it lives on a dedicated worker thread.
//!
//! This crate compiles on every host. On non-macOS targets the
//! [`Sandbox`] impl returns [`Error::Unsupported`] from the lifecycle
//! methods, so the `tokimo-packages-vm` facade can depend on it
//! unconditionally.
//!
//! Runtime requirements on macOS
//! -----------------------------
//! * The host application binary must be signed with the
//!   `com.apple.security.virtualization` entitlement. Ad-hoc signing is
//!   fine during development.

use async_trait::async_trait;
use std::path::PathBuf;
use tokimo_packages_vm_core::{
    Error, ExecOutput, ExecSpec, MountId, MountSpec, Result, Sandbox, SandboxConfig,
    SandboxId, SandboxState,
};

#[cfg(target_os = "macos")]
mod imp;
#[cfg(target_os = "macos")]
mod runtime;

pub struct MacosSandbox {
    id: SandboxId,
    config: SandboxConfig,
    state: SandboxState,
    #[cfg(target_os = "macos")]
    inner: Option<imp::Inner>,
    #[cfg(target_os = "macos")]
    serial_log: Option<PathBuf>,
}

impl MacosSandbox {
    pub fn new(config: SandboxConfig) -> Self {
        Self {
            id: SandboxId::new(),
            config,
            state: SandboxState::Created,
            #[cfg(target_os = "macos")]
            inner: None,
            #[cfg(target_os = "macos")]
            serial_log: None,
        }
    }
}

// ---------------------------- macOS impl dispatch ----------------------------

#[cfg(target_os = "macos")]
#[async_trait]
impl Sandbox for MacosSandbox {
    fn id(&self) -> SandboxId { self.id }
    fn state(&self) -> SandboxState { self.state }
    fn config(&self) -> &SandboxConfig { &self.config }

    async fn start(&mut self) -> Result<()> {
        if self.state != SandboxState::Created {
            return Err(Error::AlreadyStarted);
        }
        self.state = SandboxState::Starting;
        match imp::Inner::start(&self.config).await {
            Ok((inner, serial_log)) => {
                self.inner = Some(inner);
                self.serial_log = Some(serial_log);
                self.state = SandboxState::Running;
                Ok(())
            }
            Err(e) => {
                self.state = SandboxState::Failed;
                Err(e)
            }
        }
    }

    async fn stop(&mut self) -> Result<()> {
        if self.state != SandboxState::Running && self.state != SandboxState::Starting {
            return Ok(());
        }
        self.state = SandboxState::Stopping;
        if let Some(inner) = self.inner.take() {
            inner.stop().await?;
        }
        self.state = SandboxState::Stopped;
        Ok(())
    }

    async fn add_mount(&mut self, _spec: MountSpec) -> Result<MountId> {
        Err(Error::Unsupported(
            "hot mount add not yet implemented on macOS — declare mounts in SandboxConfig"
        ))
    }
    async fn remove_mount(&mut self, _id: MountId) -> Result<()> {
        Err(Error::Unsupported("hot mount remove not yet implemented on macOS"))
    }
    async fn exec(&self, _spec: ExecSpec) -> Result<ExecOutput> {
        Err(Error::Unsupported(
            "guest exec requires a guest agent (not yet wired up in v0.1)"
        ))
    }
    async fn wait(&mut self) -> Result<()> {
        if let Some(inner) = self.inner.as_ref() {
            inner.wait().await?;
            self.state = SandboxState::Stopped;
        }
        Ok(())
    }
    fn serial_log_path(&self) -> Option<PathBuf> { self.serial_log.clone() }
}

// ---------------------------- non-macOS stub ----------------------------

#[cfg(not(target_os = "macos"))]
#[async_trait]
impl Sandbox for MacosSandbox {
    fn id(&self) -> SandboxId { self.id }
    fn state(&self) -> SandboxState { self.state }
    fn config(&self) -> &SandboxConfig { &self.config }
    async fn start(&mut self) -> Result<()> { Err(Error::Unsupported("macOS backend only runs on macOS")) }
    async fn stop(&mut self) -> Result<()> { Err(Error::Unsupported("macOS backend only runs on macOS")) }
    async fn add_mount(&mut self, _spec: MountSpec) -> Result<MountId> { Err(Error::Unsupported("macOS backend only runs on macOS")) }
    async fn remove_mount(&mut self, _id: MountId) -> Result<()> { Err(Error::Unsupported("macOS backend only runs on macOS")) }
    async fn exec(&self, _spec: ExecSpec) -> Result<ExecOutput> { Err(Error::Unsupported("macOS backend only runs on macOS")) }
    async fn wait(&mut self) -> Result<()> { Err(Error::Unsupported("macOS backend only runs on macOS")) }
}
