//! Example: share a host directory into the guest via 9p.
//!
//! Writes `hello.txt` on the host, mounts the dir at `/mnt/tmp` inside
//! the guest, lets the guest init cat the file into the serial log,
//! then asserts we see the expected content in the log.
//!
//! Run: `cargo run --example host_dir_demo`

use std::path::PathBuf;
use std::time::Duration;
use tokimo_packages_vm::{MountSpec, SandboxConfig};

const MAGIC: &str = "hello-from-host-dir";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("tokimo=info".parse()?),
        )
        .init();

    let host_dir = PathBuf::from(std::env::temp_dir()).join("tokimo-host-dir-demo");
    let _ = std::fs::create_dir_all(&host_dir);
    std::fs::write(host_dir.join("hello.txt"), format!("{MAGIC}\n"))?;

    let mut cfg = SandboxConfig::new("host-dir-demo")
        .vcpus(1)
        .memory_mib(256)
        .mount(MountSpec::HostDir {
            tag: "hosttmp".into(),
            host_path: host_dir.clone(),
            guest_path: "/mnt/tmp".into(),
            read_only: false,
        });
    cfg.extra_cmdline.push(
        "tokimo.script=python3 -c 'print(open(\"/mnt/tmp/hello.txt\").read().strip())'".into(),
    );

    let mut sbx = tokimo_packages_vm::new_default(cfg);
    println!(">>> starting sandbox");
    sbx.start().await?;
    let log = sbx.serial_log_path().expect("serial log");
    println!(">>> serial log: {}", log.display());

    // Wait for the guest to boot and print. Debian boot takes a few seconds.
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
        anyhow::bail!("did not observe magic string in serial log");
    }
    println!(">>> success");
    Ok(())
}
