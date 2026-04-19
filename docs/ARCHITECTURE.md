# Architecture

## The problem

Users want to expose arbitrary filesystem-like data (in-memory trees,
remote buckets, SQL-backed objects) to code running inside a sandbox.
The naïve approach is to mount a host-FUSE filesystem and share that
mount into the guest; that requires FUSE/macFUSE/WinFsp on *every*
supported host OS.

tokimo-kvm flips the design: **FUSE runs inside the guest**, never on
the host. The host only has to ferry opaque bytes.

## Components

```
user code   ──▶  Arc<dyn TokimoVfs>
                      │
                      ▼
               rpc::serve_stream       (host, per-port task)
                      │
                      ▼  length-prefixed postcard frames
                      │  over a Unix socket (linux/mac) / named pipe (win)
                      ▼  attached to a virtio-serial port
               rpc::Client             (guest, opens /dev/virtio-ports/tokimo.rpc.<tag>)
                      │
                      ▼
              tokimo-agent             (guest)
                      │  fuser::Filesystem
                      ▼
                 /mnt/<tag>             (guest)
```

### Core types (`tokimo-packages-vm-core`)

* `TokimoVfs` — async trait, ten methods covering the Linux FUSE ops
  the agent actually uses (stat, list, read, write, create, mkdir,
  remove, rmdir, rename, truncate). `Send + Sync + 'static`.
* `FileAttr`, `DirEntry`, `FsKind`, `VfsError` — `serde`-friendly data
  types that cross the RPC boundary.
* `MountSpec` — `HostDir` or `Vfs`.
* `SandboxConfig` — vcpus, memory, mounts, network, image paths, extra
  cmdline. `ImagePaths::from_env_or_default` honors `TOKIMO_IMG_DIR`.
* `Sandbox` trait — lifecycle (start/stop/wait), exec, mount manipulation.

### RPC (`tokimo-packages-vm-rpc`)

Length-prefixed postcard frames over any bidirectional byte stream. The
public entry point is `serve_stream(stream, Arc<dyn TokimoVfs>)`
which runs the request/response loop on one `AsyncRead + AsyncWrite +
Send + Unpin` value. No TCP anywhere. The guest agent uses
`Client::connect_port("tokimo.rpc.<tag>")` which opens
`/dev/virtio-ports/tokimo.rpc.<tag>` and wraps it in the same framed
codec.

### Agent (`tokimo-packages-vm-agent`)

A `fuser::Filesystem` that owns a `rpc::Client`. It maintains two small
tables:

* `inode → PathBuf` (populated on `lookup`/`readdir`, with inode 1 = /).
* `fh → PathBuf` (populated on `open`/`create`/`opendir`, dropped on
  `release`).

Every op synchronously drives the async `Client` via a dedicated
`tokio::runtime::Runtime`. No caching; correctness first.

Built as `x86_64-unknown-linux-musl` with `RUSTFLAGS="-C target-feature=+crt-static"`
so it drops into any initramfs without libc compatibility worries.

### Linux backend (`tokimo-packages-vm-linux`)

1. Every mount — `MountSpec::HostDir` and `MountSpec::Vfs` alike — is
   normalized into an `Arc<dyn TokimoVfs>`. `HostDir { host_path }` is
   wrapped by the built-in `tokimo_packages_vm_core::HostFs` adapter.
2. For each mount:
   - create a Unix socket at `<runtime>/rpcN.sock`;
   - attach it to a virtio-serial port named `tokimo.rpc.<tag>` on a
     shared `virtio-serial-pci` bus;
   - `tokio::spawn(rpc::serve_stream(sock, vfs))` on every accepted
     connection.
3. Boot QEMU with `-kernel`/`-initrd`/`-drive rootfs.squashfs`,
   passing `console=ttyS0`, `root=/dev/vda rootfstype=squashfs ro`,
   and the mounts list (`tag:guest_path,...`).
4. QMP and the human monitor are exposed over Unix sockets (never TCP).

### macOS backend (`tokimo-packages-vm-macos`)

Same contract but via `Virtualization.framework` on a dedicated VM
thread (VZ objects are queue-bound and `!Send`). Every mount (HostDir
via HostFs, or user Vfs) → a `VZVirtioConsoleDeviceConfiguration` with
one `VZVirtioConsolePortConfiguration` per mount, named
`tokimo.rpc.<tag>`, attached via `VZFileHandleSerialPortAttachment`
backed by one half of a `socketpair(AF_UNIX)`. The other half is the
host-side RPC socket.

### Windows backend (`tokimo-packages-vm-wsl`)

Pure subprocess: `wsl.exe`. We never spawn a hypervisor of our own —
WSL2 already is one. Per sandbox we `wsl --import` the same Debian
rootfs as a private distro (one-time per sandbox name), then for each
mount we spawn:

```
wsl.exe -d <distro> --exec /sbin/tokimo-agent --stdio --mount /mnt/<tag> --tag <tag>
```

The child's stdin/stdout become the RPC transport — `wsl.exe`
faithfully forwards those across the host/guest boundary. From there,
`tokimo_packages_vm_rpc::serve_stream` runs unchanged with the user's
`Arc<dyn TokimoVfs>`, and the agent FUSE-mounts at `/mnt/<tag>` inside
the distro. `Sandbox::exec()` runs `wsl.exe -d <distro> --exec <prog>
<args>`. Shutdown is `wsl --terminate <distro>`.

No QEMU, no WHPX, no virtio drivers, no host FUSE/WinFsp, no TCP
ports. The host-side dependency is exactly one binary that ships with
Windows (`wsl.exe`); on a fresh box, `wsl --install` is the entire
setup.

### Guest image

A Debian bookworm squashfs built by `scripts/image/build.sh` with
`mmdebstrap`. Includes `python3`, `python3-pip`, the statically-linked
`tokimo-agent` at `/sbin/tokimo-agent`, and `/sbin/tokimo-init`. The
matching kernel (`linux-image-cloud-amd64`) ships inside the rootfs so
the needed modules (`fuse`, `virtio_console`, `virtio_net`, …) are
available. A tiny busybox initrd loads `virtio_blk` + `squashfs`,
mounts `/dev/vda` read-only over an overlay tmpfs, and
`switch_root`s into the Debian userspace. `tokimo-init` parses
`tokimo.mounts=` / `tokimo.script=` from `/proc/cmdline` and either
drops to a `bash` shell or runs the script and powers off.

## Data flow: an example read

```
guest              tokimo-agent                  virtio-serial        rpc::serve_stream   TokimoVfs
──────             ────────────                  ──────────────       ──────────────────   ─────────
open("/a.txt")  ──▶ fh alloc
read(fh,0,4096) ──▶ rpc::Client.read(path,0,4096)
                       │ postcard frame
                       ▼
                  write to /dev/virtio-ports/tokimo.rpc.<tag>
                                                  │
                                                  ▼ (chardev unix socket on host)
                                              decode frame
                                              vfs.read(path, 0, 4096).await
                                                  │
                                                  ▼ Vec<u8>
                                              encode frame, write to socket
                       ◀──────────────── bytes ───┘
                    decode Response::Read
                    ◀── bytes
reply.data(&bytes)
```

## Running tests / examples

* `cargo build --workspace --examples` — compiles on Linux / macOS /
  Windows.
* `cargo run --example host_dir_demo` — HostDir via HostFs adapter, Linux only.
* `cargo run --example vfs_memory_demo` — RPC end-to-end, Linux only.
