#!/usr/bin/env bash
# One-shot environment setup for tokimo-kvm on Linux.
#
# This script is designed so that:
#   * `sudo` is only used for what *genuinely* requires root: installing
#     system packages and (optionally, one-time) granting your user
#     access to /dev/kvm via the `kvm` group.
#   * Once this has run successfully once, running the sandbox itself
#     (`cargo run --example ...`) needs NO sudo at all.
#   * Re-running the script on an already-prepared machine is a no-op.
#
# Environment flags:
#   TOKIMO_NO_SUDO=1       never call sudo; fail if anything is missing
#   TOKIMO_KVM_MODE=group  prefer `usermod -aG kvm` (default, persistent)
#   TOKIMO_KVM_MODE=chmod  fall back to `chmod 0666 /dev/kvm` (ephemeral)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ASSETS="${TOKIMO_ASSETS:-$ROOT/assets}"
mkdir -p "$ASSETS"

NO_SUDO="${TOKIMO_NO_SUDO:-0}"
KVM_MODE="${TOKIMO_KVM_MODE:-group}"

sudo_run() {
    if [[ "$NO_SUDO" == "1" ]]; then
        echo "FATAL: TOKIMO_NO_SUDO=1 but the following needs root: $*" >&2
        exit 1
    fi
    sudo "$@"
}

if ! ls -l /dev/kvm >/dev/null 2>&1; then
    echo "FATAL: /dev/kvm not present. Enable nested virtualization."; exit 1
fi

# --- packages (needs sudo, one-time) ---
need_install=()
for pkg in virtiofsd qemu-system-x86 busybox-static cpio wget; do
    dpkg -s "$pkg" >/dev/null 2>&1 || need_install+=("$pkg")
done
if [[ ${#need_install[@]} -gt 0 ]]; then
    echo ">>> installing system packages (needs sudo, one-time): ${need_install[*]}"
    sudo_run apt-get update -qq
    sudo_run env DEBIAN_FRONTEND=noninteractive \
        apt-get install -y -qq "${need_install[@]}"
else
    echo ">>> system packages already installed, skipping sudo apt"
fi

# --- KVM permissions (no sudo needed after this, ever) ---
if [[ -r /dev/kvm && -w /dev/kvm ]]; then
    echo ">>> /dev/kvm already accessible to $(id -un), skipping"
elif [[ "$KVM_MODE" == "chmod" ]]; then
    echo ">>> granting /dev/kvm world-rw (ephemeral; resets on reboot)"
    sudo_run chmod 0666 /dev/kvm
else
    if ! getent group kvm >/dev/null; then
        echo ">>> creating 'kvm' group"
        sudo_run groupadd -r kvm
    fi
    if ! id -nG "$USER" | tr ' ' '\n' | grep -qx kvm; then
        echo ">>> adding $USER to 'kvm' group (persistent)"
        sudo_run usermod -aG kvm "$USER"
        echo
        echo "!!  You were just added to the 'kvm' group. You must log out"
        echo "!!  and back in (or run 'newgrp kvm') for cargo run to work"
        echo "!!  without sudo. If you want it to work RIGHT NOW in this"
        echo "!!  shell only, re-run this script with TOKIMO_KVM_MODE=chmod."
        echo
    fi
    # Make sure /dev/kvm is group-accessible (distro udev rules usually do
    # this already; this is a safety net).
    if [[ "$(stat -c '%G' /dev/kvm)" != "kvm" || ! "$(stat -c '%A' /dev/kvm)" =~ ^crw-rw- ]]; then
        sudo_run chgrp kvm /dev/kvm || true
        sudo_run chmod g+rw /dev/kvm || true
    fi
fi

# --- kernel + modules from Alpine linux-virt ---
ALPINE_VER="${TOKIMO_ALPINE_VER:-v3.20}"
KERNEL_URL="${TOKIMO_KERNEL_URL:-https://dl-cdn.alpinelinux.org/alpine/${ALPINE_VER}/releases/x86_64/netboot/vmlinuz-virt}"
APK_URL="${TOKIMO_APK_URL:-https://dl-cdn.alpinelinux.org/alpine/${ALPINE_VER}/main/x86_64/linux-virt-6.6.134-r0.apk}"

if [[ ! -s "$ASSETS/vmlinuz" ]]; then
    echo ">>> downloading kernel: $KERNEL_URL"
    wget -q --show-progress -O "$ASSETS/vmlinuz" "$KERNEL_URL"
fi

MOD_DIR="$ASSETS/modules"
if [[ ! -d "$MOD_DIR" || -z "$(ls -A "$MOD_DIR" 2>/dev/null)" ]]; then
    echo ">>> downloading modules apk: $APK_URL"
    tmp_apk="$(mktemp)"
    wget -q --show-progress -O "$tmp_apk" "$APK_URL"
    tmp_dir="$(mktemp -d)"
    # APKs are gzipped tar streams with a trailing signature; plain tar -xzf
    # works on rust-vmm builds of Alpine.
    tar -xzf "$tmp_apk" -C "$tmp_dir" 2>/dev/null || true
    if [[ -d "$tmp_dir/lib/modules" ]]; then
        mkdir -p "$MOD_DIR"
        cp -r "$tmp_dir/lib/modules/." "$MOD_DIR/"
    else
        echo "FATAL: failed to extract lib/modules from $APK_URL" >&2
        exit 1
    fi
    rm -rf "$tmp_dir" "$tmp_apk"
fi

# --- initramfs ---
TOKIMO_MODULES="$MOD_DIR" bash "$ROOT/scripts/build-initramfs.sh" "$ASSETS"

echo
echo "Setup OK:"
ls -la "$ASSETS"
