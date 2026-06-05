//! tokimo: cross-platform entry point.
//!
//! Re-exports the core trait + types. The `TokimoVfs` trait is the
//! user-facing FS abstraction; pass `Arc<dyn TokimoVfs>` to mounts.

pub use tokimo_packages_vm_core::*;

#[cfg(target_os = "linux")]
pub use tokimo_packages_vm_linux as linux;
#[cfg(target_os = "macos")]
pub use tokimo_packages_vm_macos as macos;
#[cfg(target_os = "windows")]
pub use tokimo_packages_vm_wsl as windows;

pub fn new_default(config: SandboxConfig) -> Box<dyn Sandbox> {
    #[cfg(target_os = "linux")]
    {
        Box::new(tokimo_packages_vm_linux::QemuSandbox::new(config))
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(tokimo_packages_vm_macos::MacosSandbox::new(config))
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(tokimo_packages_vm_wsl::WslSandbox::new(config))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        compile_error!("unsupported target OS");
    }
}
