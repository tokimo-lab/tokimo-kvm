//! Interactive REPL inside a long-running tokimo sandbox.
//!
//! Boots a Debian guest, mounts the current working directory at `/work`,
//! enables outbound networking, and drops you straight into an
//! interactive `/bin/bash` on the guest serial console. Your local
//! terminal is bridged to the guest in raw mode, so arrow keys, Ctrl-C,
//! tab completion, etc. all work.
//!
//!     $ cargo run --example repl
//!     ...
//!     >>> ready — type `exit` or Ctrl-A x to leave the guest
//!     root@tokimo:~# python3
//!     >>> import socket; socket.gethostbyname('example.com')
//!     ...
//!
//! Exit: type `exit` in the guest, or press **Ctrl-A** then **x** on the
//! host to force-stop the sandbox.
//!
//! Run: `cargo run --example repl`

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::time::Duration;
use tokimo_packages_vm::{MountSpec, SandboxConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

// ---------- termios raw-mode helper (Linux / macOS) ----------
struct RawMode {
    fd: i32,
    saved: libc::termios,
}
impl RawMode {
    fn enter() -> Option<Self> {
        unsafe {
            let fd = io::stdin().as_raw_fd();
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            // Keep OPOST so '\n' renders as CRLF on our stdout.
            raw.c_oflag |= libc::OPOST;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(RawMode { fd, saved })
        }
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("tokimo=warn".parse()?),
        )
        .init();

    let cwd = std::env::current_dir()?;
    let cfg = SandboxConfig::new("repl")
        .vcpus(2)
        .memory_mib(512)
        .interactive_serial(true)
        .mount(MountSpec::HostDir {
            tag: "work".into(),
            host_path: cwd.clone(),
            guest_path: "/work".into(),
            read_only: false,
        });

    let mut sbx = tokimo_packages_vm::new_default(cfg);
    println!(">>> booting tokimo sandbox (cwd -> /work, network on)");
    sbx.start().await?;

    let sock_path = sbx
        .serial_socket_path()
        .ok_or_else(|| anyhow::anyhow!("interactive serial not enabled"))?;
    println!(">>> serial socket: {}", sock_path.display());

    // Wait for QEMU to create the listener.
    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let stream = UnixStream::connect(&sock_path).await?;
    let (mut reader, mut writer) = stream.into_split();

    println!(">>> connected — press Ctrl-A then x to force-quit (or `exit` inside guest)\n");

    let raw = RawMode::enter();

    // Thread pumps raw bytes from real stdin → mpsc.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let stdin_thread = std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut handle = stdin.lock();
        let mut buf = [0u8; 1024];
        let mut pending_ctrla = false;
        loop {
            let n = match handle.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => n,
                Err(_) => return,
            };
            let mut out = Vec::with_capacity(n);
            for &b in &buf[..n] {
                if pending_ctrla {
                    pending_ctrla = false;
                    if b == b'x' || b == b'X' {
                        std::process::exit(0);
                    }
                    // Otherwise pass through the Ctrl-A we swallowed.
                    out.push(0x01);
                    out.push(b);
                } else if b == 0x01 {
                    // Ctrl-A
                    pending_ctrla = true;
                } else {
                    out.push(b);
                }
            }
            if !out.is_empty() && tx.blocking_send(out).is_err() {
                return;
            }
        }
    });

    // stdin -> guest
    let writer_task = tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if writer.write_all(&data).await.is_err() {
                break;
            }
            let _ = writer.flush().await;
        }
    });

    // guest -> stdout
    let mut buf = [0u8; 4096];
    let mut stdout = io::stdout();
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
            Err(_) => break,
        }
    }

    drop(raw);
    writer_task.abort();
    let _ = stdin_thread; // daemon thread; process exits on Ctrl-A x
    println!("\n>>> guest disconnected, stopping sandbox...");
    sbx.stop().await?;
    println!(">>> stopped");
    Ok(())
}
