#!/usr/bin/env bash
# Build a real Debian bookworm squashfs + matching kernel + tiny initrd.
#
# Output in img/:
#   vmlinuz        from linux-image-cloud-amd64 (inside the rootfs)
#   initrd.img     tiny busybox cpio that pivots to the squashfs
#   rootfs.squashfs debian bookworm minbase + python3 + tokimo-agent
#   SHA256SUMS
#
# Requires: mmdebstrap, mksquashfs, cpio, zstd, rustup with the
# x86_64-unknown-linux-musl target, and sudo (for mmdebstrap & the
# squashfs packaging step so xattrs & ownership are preserved).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${TOKIMO_IMG_DIR:-$ROOT/img}"
mkdir -p "$OUT"

KERNEL_OUT="$OUT/vmlinuz"
INITRD_OUT="$OUT/initrd.img"
SQUASH_OUT="$OUT/rootfs.squashfs"
SUMS_OUT="$OUT/SHA256SUMS"

# --------------------------------------------------------------------
# Freshness check: if everything exists and is newer than every source
# that affects the image, skip.
# --------------------------------------------------------------------
is_fresh() {
    [[ -s "$KERNEL_OUT" && -s "$INITRD_OUT" && -s "$SQUASH_OUT" ]] || return 1
    local newest_src
    newest_src=$(find "$ROOT/packages/tokimo-packages-vm-agent" \
                      "$ROOT/packages/tokimo-packages-vm-core" \
                      "$ROOT/packages/tokimo-packages-vm-rpc" \
                      "$ROOT/scripts/image/build.sh" \
                 -type f -newer "$SQUASH_OUT" -print -quit 2>/dev/null || true)
    [[ -z "$newest_src" ]]
}

if [[ "${TOKIMO_IMG_FORCE:-0}" != "1" ]] && is_fresh; then
    echo ">>> img/ is already fresh — skipping rebuild (TOKIMO_IMG_FORCE=1 to override)"
    exit 0
fi

# --------------------------------------------------------------------
# Preauthorise sudo (the user is expected to have done `sudo -v`).
# --------------------------------------------------------------------
if ! sudo -n true 2>/dev/null; then
    echo "FATAL: sudo is required but not cached; run 'sudo -v' first." >&2
    exit 1
fi
# Keep sudo alive for the length of the long mmdebstrap step.
(
    while true; do sudo -n true 2>/dev/null || exit; sleep 60; done
) &
SUDO_KEEPER_PID=$!
trap 'kill $SUDO_KEEPER_PID 2>/dev/null; sudo -n rm -rf "$WORK" 2>/dev/null' EXIT

# Always operate in /tmp (heavy IO; should not live under /mnt/c on WSL).
WORK="$(mktemp -d -p /tmp tokimo-build-XXXX)"
ROOTFS="$WORK/rootfs"

echo ">>> work dir: $WORK"

# --------------------------------------------------------------------
# 1. Build the static musl agent (before mmdebstrap so a failure here
#    doesn't waste the chroot).
# --------------------------------------------------------------------
echo ">>> building tokimo-agent (x86_64-unknown-linux-musl, release)"
rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1 || true
(
    cd "$ROOT"
    RUSTFLAGS="-C target-feature=+crt-static" \
    cargo build --release -p tokimo-packages-vm-agent \
        --target x86_64-unknown-linux-musl 1>&2
)
AGENT_BIN="$ROOT/target/x86_64-unknown-linux-musl/release/tokimo-agent"
[[ -x "$AGENT_BIN" ]] || { echo "FATAL: agent did not build" >&2; exit 1; }

# --------------------------------------------------------------------
# 2. mmdebstrap a Debian bookworm minbase root, with the kernel and
#    python inside so /lib/modules/<kver>/overlay.ko is available.
# --------------------------------------------------------------------
echo ">>> mmdebstrap bookworm (this takes 1-3 minutes)"
sudo mmdebstrap \
    --variant=minbase \
    --include=python3,python3-pip,ca-certificates,iproute2,udev,kmod,busybox-static,libfuse3-3,linux-image-amd64,procps,mount,dbus,less,iputils-ping \
    --components=main \
    bookworm "$ROOTFS" \
    http://deb.debian.org/debian

# Make it owned by us so the remaining steps can run unprivileged where
# possible. We'll re-chown to root before packing.
sudo chown -R "$(id -u):$(id -g)" "$ROOTFS"

# --------------------------------------------------------------------
# 3. Install the agent + the init script.
# --------------------------------------------------------------------
install -m 0755 "$AGENT_BIN" "$ROOTFS/sbin/tokimo-agent"

KVER=$(basename "$(ls -d "$ROOTFS"/lib/modules/*/ | head -n 1)")
echo ">>> kernel version: $KVER"

# Ensure modules.dep/alias/symbols exist so `modprobe` can resolve deps.
sudo chroot "$ROOTFS" depmod -a "$KVER" 2>/dev/null \
    || depmod -b "$ROOTFS" "$KVER" 2>/dev/null \
    || true

cat > "$ROOTFS/sbin/tokimo-init" <<'INITEOF'
#!/bin/sh
# (No `set -e`: we want to keep running even if individual guest-side
# steps fail, so the serial log keeps useful diagnostics.)
/bin/mount -t proc  proc  /proc 2>/dev/null || true
/bin/mount -t sysfs sysfs /sys 2>/dev/null || true
/bin/mount -t devtmpfs dev /dev 2>/dev/null || true
/bin/mount -t tmpfs tmp /tmp 2>/dev/null || true
/bin/mkdir -p /dev/pts /dev/shm /run
/bin/mount -t devpts -o newinstance,ptmxmode=0666 devpts /dev/pts 2>/dev/null || true
/bin/mount -t tmpfs tmpfs /run 2>/dev/null || true

echo ""
echo "===== tokimo-init (debian) ====="

# Load modules if not built-in. fuse is the only one we truly need for
# mount; virtio_console / virtio_net are useful best-effort.
for m in fuse virtio_console virtio_net; do
    /sbin/modprobe -q "$m" 2>/dev/null || true
done

# Bring up network (user-mode NAT).
ip link set lo up 2>/dev/null || true
if ip link show eth0 >/dev/null 2>&1; then
    ip link set eth0 up 2>/dev/null || true
    udhcpc -i eth0 -q -n -t 3 >/dev/null 2>&1 \
        || dhclient eth0 2>/dev/null \
        || ip addr add 10.0.2.15/24 dev eth0 2>/dev/null || true
    ip route add default via 10.0.2.2 dev eth0 2>/dev/null || true
fi

# Parse /proc/cmdline
mounts_arg=""
shell_mode=0
for w in $(cat /proc/cmdline); do
    case "$w" in
        tokimo.mounts=*) mounts_arg="${w#tokimo.mounts=}" ;;
        tokimo.shell=1)  shell_mode=1 ;;
    esac
done
script_cmd=$(sed -n 's/.*tokimo\.script=\(.*\)$/\1/p' /proc/cmdline)

if [ -n "$mounts_arg" ]; then
    old_ifs="$IFS"
    IFS=","
    for pair in $mounts_arg; do
        IFS=":" read -r tag mpath rest <<EOF_PAIR
$pair
EOF_PAIR
        /bin/mkdir -p "$mpath"
        if ! [ -x /sbin/tokimo-agent ]; then
            echo "tokimo-init: /sbin/tokimo-agent not found or not executable"
        fi
        /sbin/tokimo-agent --port-name "tokimo.rpc.${tag}" --mount "$mpath" --tag "$tag" \
            >/var/log/tokimo-agent-${tag}.log 2>&1 &
        agent_pid=$!
        # Wait up to ~15s for the FUSE mount to materialise
        for i in $(seq 1 75); do
            if /bin/grep -q " $mpath fuse" /proc/mounts; then
                echo "tokimo-init: mount $tag -> $mpath"
                break
            fi
            sleep 0.2
        done
        if ! /bin/grep -q " $mpath fuse" /proc/mounts; then
            echo "tokimo-init: mount FAILED $tag -> $mpath (agent pid=$agent_pid)"
            if ! kill -0 "$agent_pid" 2>/dev/null; then
                echo "  agent process exited"
            fi
            echo "  /sys/class/virtio-ports:"
            ls /sys/class/virtio-ports/ 2>&1 | sed 's/^/    /'
            for vp in /sys/class/virtio-ports/*/name; do
                [ -e "$vp" ] && echo "    $vp = $(cat $vp)"
            done
            echo "  /dev/vport* :"
            ls /dev/vport* 2>&1 | sed 's/^/    /'
            echo "  /proc/modules (virtio_console):"
            grep -E 'virtio_console|fuse' /proc/modules 2>&1 | sed 's/^/    /'
            if [ -s /var/log/tokimo-agent-${tag}.log ]; then
                sed 's/^/  agent: /' /var/log/tokimo-agent-${tag}.log
            else
                echo "  agent log empty"
            fi
        fi
        IFS=","
    done
    IFS="$old_ifs"
fi

if [ -n "$script_cmd" ]; then
    echo "tokimo-init: running tokimo.script"
    /bin/sh -c "$script_cmd"
    rc=$?
    sync
    echo "tokimo-init: script exit=$rc"
    if [ "$shell_mode" = "1" ]; then
        echo "===== dropping to shell ====="
        exec /bin/bash -i </dev/console >/dev/console 2>&1
    fi
    sleep 0.5
    poweroff -f 2>/dev/null || /bin/busybox poweroff -f 2>/dev/null || halt -f
    exit 0
fi

if [ "$shell_mode" = "1" ]; then
    echo "===== tokimo shell ready ====="
    exec /bin/bash -i </dev/console >/dev/console 2>&1
fi

echo "===== ready ====="
exec /bin/bash -i </dev/console >/dev/console 2>&1
INITEOF
chmod +x "$ROOTFS/sbin/tokimo-init"

# Ensure /var/log exists for the agent logs written at runtime.
mkdir -p "$ROOTFS/var/log"

# --------------------------------------------------------------------
# 4. Kernel extraction — KVER was determined above.
# --------------------------------------------------------------------
cp "$ROOTFS/boot/vmlinuz-$KVER" "$KERNEL_OUT"

# --------------------------------------------------------------------
# 5. Build the initrd. Busybox + a simple /init that:
#    - mounts /proc /sys /dev
#    - loads overlay.ko
#    - mounts /dev/vda (squashfs) at /newroot
#    - overlays a tmpfs upper over it at /overlay/merged
#    - switch_root to /sbin/tokimo-init
# --------------------------------------------------------------------
echo ">>> building initrd"
INITRD="$WORK/initrd"
mkdir -p "$INITRD"/{bin,sbin,etc,proc,sys,dev,newroot,overlay,mnt,tmp,lib/modules}

BUSYBOX="$(command -v busybox)"
cp "$BUSYBOX" "$INITRD/bin/busybox"
chmod +x "$INITRD/bin/busybox"
for a in sh ls cat mount umount mkdir echo sleep poweroff reboot ln cp sed grep cut awk \
         lsmod modprobe insmod dmesg find stat head tail switch_root mknod touch rm chmod; do
    ln -sf busybox "$INITRD/bin/$a"
done

# Pull modules needed before pivot: virtio stack, virtio_blk,
# squashfs, and overlay. Cloud kernel: everything except virtio_ring
# and virtio are modular.
KERN_MODDIR="$ROOTFS/lib/modules/$KVER/kernel"
copy_ko() {
    local src="$1"; local dst="$INITRD/modules/$(basename "$src")"
    if [[ -f "$src" ]]; then cp "$src" "$dst"
    elif [[ -f "${src}.xz" ]]; then xzcat "${src}.xz" > "$dst"
    else echo "WARN: $src not found in rootfs" >&2; fi
}
mkdir -p "$INITRD/modules"
copy_ko "$KERN_MODDIR/drivers/virtio/virtio.ko"
copy_ko "$KERN_MODDIR/drivers/virtio/virtio_ring.ko"
copy_ko "$KERN_MODDIR/drivers/virtio/virtio_pci.ko"
copy_ko "$KERN_MODDIR/drivers/virtio/virtio_pci_modern_dev.ko"
copy_ko "$KERN_MODDIR/drivers/virtio/virtio_pci_legacy_dev.ko"
copy_ko "$KERN_MODDIR/drivers/block/virtio_blk.ko"
copy_ko "$KERN_MODDIR/fs/squashfs/squashfs.ko"
copy_ko "$KERN_MODDIR/fs/overlayfs/overlay.ko"

cat > "$INITRD/init" <<'IEOF'
#!/bin/sh
PATH=/bin:/sbin
export PATH
/bin/mount -t proc proc /proc
/bin/mount -t sysfs sysfs /sys
/bin/mount -t devtmpfs dev /dev 2>/dev/null || true

# Load kernel modules required to discover the root block device and
# mount the squashfs rootfs. Order matters: virtio, virtio_ring, then
# the transports, then virtio_blk, then squashfs.
for m in virtio virtio_ring virtio_pci_legacy_dev virtio_pci_modern_dev \
         virtio_pci virtio_blk squashfs overlay; do
    if [ -f "/modules/$m.ko" ]; then
        /bin/insmod "/modules/$m.ko" 2>/dev/null || true
    fi
done

# Wait briefly for /dev/vda to appear.
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    [ -b /dev/vda ] && break
    sleep 0.1
done

mkdir -p /newroot /overlay
if ! /bin/mount -t squashfs -o ro /dev/vda /newroot; then
    echo "FATAL: cannot mount /dev/vda as squashfs" >&2
    exec /bin/sh
fi

if /bin/mount -t tmpfs tmpfs /overlay; then
    mkdir -p /overlay/upper /overlay/work /overlay/merged
    if /bin/mount -t overlay overlay -o lowerdir=/newroot,upperdir=/overlay/upper,workdir=/overlay/work /overlay/merged; then
        ROOT=/overlay/merged
    else
        ROOT=/newroot
    fi
else
    ROOT=/newroot
fi

mkdir -p "$ROOT/proc" "$ROOT/sys" "$ROOT/dev"

exec /bin/switch_root "$ROOT" /sbin/tokimo-init
IEOF
chmod +x "$INITRD/init"

(
    cd "$INITRD"
    find . -print0 | cpio --null -o --format=newc --quiet | gzip -9 > "$INITRD_OUT"
)
ls -lh "$INITRD_OUT"

# --------------------------------------------------------------------
# 6. Pack squashfs. Chown to root first so ownership inside the image
#    is sane; mksquashfs itself doesn't need root if it has all its
#    inputs readable.
# --------------------------------------------------------------------
echo ">>> packing squashfs (zstd level 15)"
sudo chown -R 0:0 "$ROOTFS"
sudo mksquashfs "$ROOTFS" "$SQUASH_OUT" \
    -comp zstd -Xcompression-level 15 \
    -noappend -no-progress 1>&2
sudo chown "$(id -u):$(id -g)" "$SQUASH_OUT"
ls -lh "$SQUASH_OUT"

# Also emit rootfs.tar for the Windows (WSL2) backend. wsl --import
# only accepts a tarball (gzipped or plain). We keep it uncompressed
# here so the same artifact can be reused verbatim for debugging; the
# pack step may gzip it before shipping.
TAR_OUT="$OUT/rootfs.tar"
sudo tar --numeric-owner -C "$ROOTFS" -cf "$TAR_OUT" .
sudo chown "$(id -u):$(id -g)" "$TAR_OUT"
ls -lh "$TAR_OUT"

# --------------------------------------------------------------------
# 7. Checksums.
# --------------------------------------------------------------------
(cd "$OUT" && sha256sum vmlinuz initrd.img rootfs.squashfs rootfs.tar > SHA256SUMS)
echo ">>> image built in $OUT"
ls -lh "$OUT"
