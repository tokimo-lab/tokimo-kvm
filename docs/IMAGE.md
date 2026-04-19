# Guest image: build, customize, distribute

## What's in the image?

The guest is a **Debian 13 (trixie) squashfs** with:

| component            | role                                                   |
| -------------------- | ------------------------------------------------------ |
| `linux-image-cloud-amd64` | kernel — has fuse/virtio-console/virtio-net         |
| `python3` + `python3-pip`| run user / AI-agent scripts                        |
| `ca-certificates`    | TLS                                                    |
| `iproute2`, `procps`, `mount`, `busybox-static`, `iputils-ping` | basic user-land |
| `libfuse3-3`         | FUSE userspace lib                                     |
| `/sbin/tokimo-agent` | statically-linked (musl) FUSE↔RPC bridge               |
| `/sbin/tokimo-init`  | tiny `/bin/sh` script: brings up network, mounts FUSE, runs `tokimo.script=`, optionally `exec /bin/bash` |

Artifacts produced in `img/`:

| file             | size   | purpose                                      |
| ---------------- | ------ | -------------------------------------------- |
| `vmlinuz`        | ~10 MB | extracted from the Debian kernel package (Linux backend) |
| `initrd.img`     | ~3 MB  | minimal initramfs → `switch_root` into squashfs (Linux)   |
| `rootfs.squashfs`| ~220 MB| zstd-compressed Debian rootfs (Linux / macOS backends)   |
| `rootfs.tar`     | ~500 MB| uncompressed tar of the same rootfs (**Windows/WSL2 backend only**) |
| `SHA256SUMS`     | -      | for `scripts/image/verify.sh`                |

The Windows backend (`tokimo-packages-vm-wsl`) consumes `rootfs.tar`
directly via `wsl --import`. It ignores `vmlinuz`/`initrd.img`/`squashfs`
(WSL2 supplies its own kernel). Linux and macOS backends ignore the `.tar`.

## Build

```bash
bash scripts/image/build.sh
```

- Takes ~14 min cold (mmdebstrap pulls Debian).
- Needs **sudo one time** (`mmdebstrap` + `mksquashfs` set ownership
  inside the tree). Running the sandbox afterwards does **not** need sudo.
- Idempotent — a subsequent run skips the rebuild unless you pass
  `TOKIMO_IMG_FORCE=1` or touch `scripts/image/build.sh`.

Env knobs:

- `TOKIMO_IMG_DIR=<dir>` — destination, default `./img/`.
- `TOKIMO_IMG_FORCE=1` — rebuild even if already fresh.

## Change what's inside the image

Edit `scripts/image/build.sh`:

- **Add packages:** extend the `--include=…` comma list on the
  `mmdebstrap` line (around line 87).
  Example — add curl + git:

  ```
  --include=python3,python3-pip,ca-certificates,iproute2,udev,kmod,\
  busybox-static,libfuse3-3,linux-image-amd64,procps,mount,dbus,less,\
  iputils-ping,curl,git
  ```

- **Ship files / pre-built binaries:** copy them into `$ROOTFS/...`
  after the mmdebstrap call (roughly line 95+ onwards). They're
  preserved because squashfs packaging runs under sudo.

- **Change guest init behavior:** edit the big `cat > "$ROOTFS/sbin/tokimo-init" <<'INITEOF'` heredoc. Supports:
  - `tokimo.mounts=tag:path,…` — FUSE mounts
  - `tokimo.script=<shell>`    — one-shot batch command then poweroff
  - `tokimo.shell=1`           — drop into bash on the serial console
    (set automatically when you configure `interactive_serial(true)`)

After edits, either re-run `bash scripts/image/build.sh` (full rebuild)
or use the **fast patch path** for init-script-only changes (~30 s):

```bash
sudo unsquashfs -d /tmp/edit img/rootfs.squashfs
sudo $EDITOR /tmp/edit/sbin/tokimo-init
sudo mksquashfs /tmp/edit img/rootfs.squashfs -noappend -comp zstd
sudo chown $USER img/rootfs.squashfs
(cd img && sha256sum rootfs.squashfs vmlinuz initrd.img > SHA256SUMS)
```

## Does the guest have internet?

**Yes, by default.** `NetworkSpec::default()` enables QEMU/slirp
user-mode NAT; the guest gets DHCP, DNS, and outbound TCP/UDP. The
init script auto-configures eth0 (`udhcpc` → `dhclient` → static
10.0.2.15 fallback) and the default gateway. No host ports are opened
unless you add `PortForward`s.

Verified with `cargo run --example network_demo`: DNS lookup, raw TCP
to `1.1.1.1:80`, and `http://example.com/` → HTTP 200.

To disable: `cfg.network = NetworkSpec { user_mode: false, ..Default::default() };`

## Portability (use on a different machine)

The three files in `img/` are **fully portable x86_64 artifacts**:

- Copy `img/{vmlinuz,initrd.img,rootfs.squashfs,SHA256SUMS}` to the
  new machine (or stash them anywhere and set `TOKIMO_IMG_DIR`).
- The new machine needs `qemu-system-x86_64` + KVM (or WHPX / VZ on
  other OSes), but it does **not** need `mmdebstrap` or any of the
  build-time tools.

Or rebuild locally: `bash scripts/image/build.sh` works on any recent
Debian/Ubuntu/WSL2 host.

## Distribute

```bash
bash scripts/image/pack.sh   # produces dist/tokimo-image-<sha>.tar.zst
```

Recipients extract the tar anywhere, point `TOKIMO_IMG_DIR` at it,
then run `bash scripts/image/verify.sh` (checks SHA256SUMS).
