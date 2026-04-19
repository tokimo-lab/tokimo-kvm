# tokimo-packages-vm

> Long-running, lightweight VM sandbox for running AI-agent-generated
> scripts — cross-platform (Linux / macOS / Windows), with a **pluggable
> Rust VFS** and no host-side FUSE dependency.

Typical use: your Python agent spits out a script, you run it inside a
fresh Debian guest with a virtual filesystem you control (in-memory,
SQL-backed, S3-backed, or simply your local `./workspace`). You get
process isolation + filesystem isolation + optional network — and the
sandbox stays up so follow-up scripts can reuse it.

---

## TL;DR

```bash
# one-time: build the guest image (Debian 13 + python3 + tokimo-agent)
bash scripts/image/build.sh

# run demos (no sudo needed from here on)
cargo run --example host_dir_demo    # mount a host dir -> guest reads a file
cargo run --example vfs_memory_demo  # mount a pure-Rust in-memory VFS
cargo run --example network_demo     # verify outbound internet works
cargo run --example repl             # interactive bash inside the sandbox
```

Ctrl-A then **x** exits the REPL. Everything else (arrow keys, Ctrl-C,
`python3`, `pip3`, `curl` via `python -m http.client`, …) behaves as a
normal terminal.

---

## Architecture at a glance

```
┌──────────────── host (linux / macos / windows) ────────────────┐
│  your app                                                       │
│  ├─ Arc<dyn TokimoVfs>            ← you implement (or use HostFs)
│  └─ tokimo_packages_vm::new_default(SandboxConfig)              │
│                                                                 │
│  per-OS backend                                                 │
│  ├─ Linux: qemu+kvm, virtio-serial ⇄ Unix socket                │
│  ├─ macOS: VZ, virtio-console port ⇄ socketpair                 │
│  ├─ Windows: `wsl.exe -d … --exec tokimo-agent --stdio`         │
│  ├─ per mount: stream ⇄ tokimo_packages_vm_rpc::serve_stream    │
│  └─ passes tokimo.mounts=tag:path,... on kernel cmdline (L/M)   │
│                                                                 │
│                    ↕ virtio-serial / wsl-stdio (no TCP, no host FUSE)
├─────────────────────────────────────────────────────────────────┤
│  guest (Debian 13 squashfs, x86_64, python3 pre-installed)     │
│  /sbin/tokimo-init spawns one tokimo-agent per mount            │
│  tokimo-agent: fuser::Filesystem → RPC Client → host VFS       │
│  Result: /mnt/<tag> is a real FUSE mount inside the guest      │
└─────────────────────────────────────────────────────────────────┘
```

**One transport for everything.** Both `MountSpec::HostDir` (wrapped
automatically in a built-in `HostFs` adapter) and `MountSpec::Vfs`
(your Rust VFS) travel the same path: host → virtio-serial → guest
FUSE. No 9p, no virtiofs, no host FUSE, no macFUSE, no WinFsp, no TCP
ports bound on the host.

---

## Platform requirements

### Linux / WSL2 (fully tested)
- Rust stable
- `qemu-system-x86_64`, `mmdebstrap`, `mksquashfs`, `cpio`, `zstd`,
  `busybox-static` (for image build only)
- `/dev/kvm` readable by your user (`sudo usermod -aG kvm $USER`)
- Image build needs one-time sudo; **running the sandbox does not**

### macOS (compile-only in this build)
- Rust stable + `aarch64-apple-darwin` / `x86_64-apple-darwin`
- **Virtualization.framework** is built into macOS 12+. No brew install,
  no signing gymnastics beyond a debug entitlement for your own binary.
- No QEMU required on macOS — we use VZ directly.

### Windows (compile-only in this build — runs on Windows 10 21H2+ / 11)
> **Does the user need to install anything?** No separate VMM — we
> use **WSL2**, which is a default platform component on modern
> Windows. On a fresh machine: `wsl --install` (one command, pulls
> the kernel automatically). On systems where WSL2 is already set
> up (which is most developer boxes), **zero install**.

- **How it works:** on Windows we don't spawn QEMU at all. Instead,
  `tokimo-packages-vm-wsl` imports our Debian `rootfs.tar` as a
  dedicated WSL distro (`wsl --import tokimo-<id> …`) and launches
  `tokimo-agent --stdio` inside it via `wsl.exe`. The RPC runs over
  the child's stdin/stdout — no TCP ports, no virtio drivers, no
  host FUSE.
- **Required:**
  - WSL2 installed (`wsl --status` should report `Default Version: 2`).
    First-time setup: `wsl --install` + reboot once.
  - `img/rootfs.tar` present (the image build also emits this now,
    alongside `rootfs.squashfs` used by the Linux backend).
- **Not required:** QEMU, Hyper-V GUI, WinFsp, admin rights after
  the initial `wsl --install`, or anything from the Microsoft Store.
- **Not supported:** Windows Home + pre-21H2 (no WSL2). Users there
  would need the QEMU backend; we removed it to simplify the tree.

> **Why WSL2?** It's pre-installed on every modern Windows dev box,
> has a real Linux kernel with KVM-like performance, supports FUSE
> inside the distro (so our agent can mount the proxied VFS), and
> gives us a single-dependency story (`wsl.exe`) that Microsoft
> actively maintains.

---

## 1. Quick start: mount a host directory

```rust
use tokimo_packages_vm::{SandboxConfig, MountSpec};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = SandboxConfig::new("demo")
        .vcpus(2).memory_mib(512)
        .mount(MountSpec::HostDir {
            tag: "work".into(),
            host_path: "./workspace".into(),
            guest_path: "/work".into(),
            read_only: false,
        });
    let mut sbx = tokimo_packages_vm::new_default(cfg);
    sbx.start().await?;
    // ... run scripts, keep sbx alive as long as you like ...
    sbx.stop().await?;
    Ok(())
}
```

## 2. Bring your own VFS

```rust
use std::sync::Arc;
use async_trait::async_trait;
use tokimo_packages_vm::{TokimoVfs, FileAttr, FsKind, DirEntry, VfsResult, MountSpec};

struct MyVfs;
#[async_trait]
impl TokimoVfs for MyVfs {
    async fn stat(&self, p: &std::path::Path) -> VfsResult<FileAttr> { /* … */ }
    async fn list(&self, p: &std::path::Path) -> VfsResult<Vec<DirEntry>> { /* … */ }
    // read / write / create / mkdir / remove / rmdir / rename / truncate
}

let vfs: Arc<dyn TokimoVfs> = Arc::new(MyVfs);
let cfg = SandboxConfig::new("demo").mount(MountSpec::Vfs {
    tag: "mem".into(), vfs, guest_path: "/mnt/mem".into(), read_only: false,
});
```

See `examples/vfs_memory_demo.rs` for a full in-memory implementation.

## 3. Interactive REPL (local debugging)

```bash
cargo run --example repl
```

Boots a guest, mounts the current directory at `/work`, enables
outbound network, and hands you a live bash prompt. Arrow keys, Ctrl-C,
tab completion, `python3`, `pip3 install …`, etc. all work. **Ctrl-A
then x** force-stops the sandbox. Because the sandbox is long-lived,
you can leave it running and connect separately via
`socat - UNIX-CONNECT:/tmp/tokimo-repl-<pid>/serial.sock` from another
shell if you want to drive it programmatically.

## 4. Outbound network (default: ON)

`NetworkSpec::default()` enables QEMU user-mode NAT (slirp). Guest gets
DHCP, DNS, and outbound TCP/UDP for free. No host ports are opened.
Tested in `network_demo` — DNS resolves, TCP connect works, HTTP 200
from `example.com`.

To turn it off or add port forwards:

```rust
use tokimo_packages_vm::{NetworkSpec, PortForward, Protocol};
cfg.network = NetworkSpec {
    user_mode: true,
    port_forwards: vec![PortForward {
        host_port: 18080, guest_port: 80, protocol: Protocol::Tcp,
    }],
    dns: None,
};
// or: NetworkSpec { user_mode: false, .. } to fully isolate
```

---

## Images

See **[docs/IMAGE.md](docs/IMAGE.md)** for the whole story. One-liner
summary:

- The image is a custom Debian 13 squashfs with `python3`, `ca-certificates`,
  `iproute2`, `libfuse3`, and the statically-linked `tokimo-agent`.
- Build it with `bash scripts/image/build.sh` (one-time, needs sudo).
- Output lands in `img/` — three files (`vmlinuz`, `initrd.img`,
  `rootfs.squashfs`, ~220 MB total). Copy them to another machine and
  they just work (the guest kernel is fully portable x86_64).
- Customize: edit `scripts/image/build.sh` (the `--include=` list and
  the init-script heredoc) and re-run. Incremental changes to the init
  script can use the `unsquashfs → edit → mksquashfs` fast path.

---

## Debug & iteration

See **[docs/DEBUG.md](docs/DEBUG.md)**. Highlights:

- Fastest inner loop: `cargo run --example repl` (live shell).
- Non-interactive sandbox? Tail `<runtime_dir>/serial.log`.
- QEMU HMP monitor: `socat - UNIX-CONNECT:<runtime_dir>/monitor.sock`.
- Cold start is ~7 s on KVM. Keep the sandbox up across many `exec`s.

---

## Cross-OS matrix

| Feature             | Linux (KVM)       | macOS (VZ)              | Windows (WSL2)          |
| ------------------- | ----------------- | ----------------------- | ----------------------- |
| HostDir share       | virtio-serial RPC | virtio-console RPC      | wsl.exe stdio RPC       |
| Vfs (guest-FUSE)    | ✅ virtio-serial  | ✅ virtio-console port  | ✅ `--stdio` agent      |
| Transport backing   | Unix socket       | `socketpair()` + fd     | Child stdin/stdout      |
| Interactive serial  | ✅ Unix socket    | (framework file handle) | `wsl -d … -- bash -i`   |
| User-mode network   | slirp (default on)| VZ NAT                  | WSL2 NAT (default on)   |
| Host TCP ports      | **none**          | **none**                | **none**                |
| External binary dep | qemu-system-x86_64| none (VZ is built in)   | `wsl.exe` (built in)    |
| Runtime-tested      | **yes**           | compile-only            | compile-only            |

---

## Repository layout

```
packages/
  tokimo-packages-vm/          facade crate (re-exports per-OS backend)
  tokimo-packages-vm-core/     traits + config types
  tokimo-packages-vm-rpc/      postcard-framed VFS RPC
  tokimo-packages-vm-agent/    guest FUSE→RPC bridge (musl-static)
  tokimo-packages-vm-linux/    QEMU + KVM backend
  tokimo-packages-vm-macos/    Virtualization.framework backend
  tokimo-packages-vm-wsl/      WSL2 backend (Windows, default)
examples/
  host_dir_demo.rs    vfs_memory_demo.rs    repl.rs    network_demo.rs
scripts/image/build.sh   build kernel + initrd + squashfs + rootfs.tar + agent
docs/
  ARCHITECTURE.md     deep dive on the RPC + transport design
  DEBUG.md            serial console, monitor, strace recipes
  IMAGE.md            image build / customize / distribute
```

## Current limitations

- `Sandbox::exec()` is not yet wired (returns `Unsupported`). For now
  use `SandboxConfig::extra_cmdline.push("tokimo.script=…")` for batch
  invocations or the REPL for interactive use.
- macOS NAT port-forwarding isn't exposed by `VZNATNetworkDeviceAttachment`;
  the guest must initiate TCP to the host.
- mac/Windows backends compile cleanly but haven't been runtime-tested
  in this tree (see matrix).
