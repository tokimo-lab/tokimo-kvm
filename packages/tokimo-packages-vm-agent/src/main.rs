//! tokimo-agent: guest-side FUSE→RPC bridge.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("tokimo-agent: linux only");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod imp;

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> { imp::run() }
