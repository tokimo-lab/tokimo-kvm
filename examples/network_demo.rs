//! Example: verify the guest has outbound internet via QEMU user-mode NAT.
//!
//! By default `NetworkSpec::default()` enables user-mode networking with
//! DNS, no host port forwards, and no host ports opened.
//!
//! Run: `cargo run --example network_demo`

use std::time::Duration;
use tokimo_packages_vm::SandboxConfig;

const MAGIC: &str = "NET-OK:";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
            .add_directive("tokimo=info".parse()?))
        .init();

    let mut cfg = SandboxConfig::new("network-demo")
        .vcpus(1)
        .memory_mib(256);

    // Python script that probes outbound connectivity without needing
    // any extra packages. We try:
    //   1. a DNS A-record lookup for example.com (via /etc/hosts? no, via getaddrinfo)
    //   2. a TCP connect to 1.1.1.1:80 (no DNS needed)
    //   3. an HTTP HEAD to http://example.com/ (DNS + TCP + HTTP)
    let probe = r#"
import socket, urllib.request
def ok(msg): print('NET-OK:'+msg, flush=True)
def fail(msg): print('NET-FAIL:'+msg, flush=True)
try:
    ip = socket.gethostbyname('example.com')
    ok('dns='+ip)
except Exception as e: fail('dns '+repr(e))
try:
    s = socket.create_connection(('1.1.1.1', 80), timeout=5); s.close()
    ok('tcp=1.1.1.1:80')
except Exception as e: fail('tcp '+repr(e))
try:
    r = urllib.request.urlopen('http://example.com/', timeout=5)
    ok('http='+str(r.status))
except Exception as e: fail('http '+repr(e))
"#;
    use base64::{engine::general_purpose::STANDARD, Engine};
    let b64 = STANDARD.encode(probe);
    cfg.extra_cmdline.push(format!(
        "tokimo.script=python3 -c 'import base64,sys; exec(base64.b64decode(\"{b64}\").decode())'"
    ));

    let mut sbx = tokimo_packages_vm::new_default(cfg);
    println!(">>> starting sandbox (user-mode NAT enabled by default)");
    sbx.start().await?;
    let log = sbx.serial_log_path().expect("serial log");
    println!(">>> serial log: {}", log.display());

    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut last_len = 0usize;
    let mut done = false;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(s) = std::fs::read_to_string(&log) {
            if s.len() > last_len { last_len = s.len(); }
            if s.contains("script exit=") {
                println!("\n===== serial output =====\n{s}\n========================");
                let ok_count = s.matches(MAGIC).count();
                let fail_count = s.matches("NET-FAIL:").count();
                println!(">>> OK={ok_count}  FAIL={fail_count}");
                done = true;
                break;
            }
        }
    }
    sbx.stop().await?;
    if !done { anyhow::bail!("guest script did not finish in time"); }
    println!(">>> done");
    Ok(())
}
