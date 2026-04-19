# Debugging tokimo-kvm

No sshd runs inside the guest image. All inspection happens via the
serial console and QEMU's monitor socket — both local Unix sockets,
never network ports.

## 1. Interactive shell (fastest inner loop)

```bash
cargo run --example repl
```

- Boots the guest, mounts `$CWD` at `/work` (read-write), enables
  outbound network, and drops you straight into `/bin/bash -i`.
- Your local terminal is put into **raw mode** and bridged to the
  guest serial console, so arrow keys, Ctrl-C, Ctrl-D, tab completion,
  `python3`, `pip3`, etc. all behave normally.
- **Exit:** type `exit` in the guest, **or** press **Ctrl-A x** on the
  host to force-stop the sandbox.

Need two windows at once? The serial console is a Unix socket; attach a
second reader with:

```bash
socat - UNIX-CONNECT:/tmp/tokimo-repl-<pid>/serial.sock
```

(Both sides see the same stream, multiplexed.)

## 2. Tail the serial log (non-interactive sandboxes)

Every non-interactive sandbox writes its full boot log + every byte
printed on the serial TTY to `<runtime_dir>/serial.log`. The path is
returned by `Sandbox::serial_log_path()` and printed by each example
program:

```bash
tail -F /tmp/tokimo-<name>-<pid>/serial.log
```

`tools/console <runtime_dir>` is a thin wrapper.

## 3. QEMU human monitor

```bash
socat - UNIX-CONNECT:/tmp/tokimo-<name>-<pid>/monitor.sock
```

Gives you the QEMU HMP prompt. Useful commands: `info status`,
`info network`, `info block`, `sendkey`, `screendump`.

## 4. Verify no host TCP ports opened

```bash
ss -tln | sort > before.txt
cargo run --example vfs_memory_demo &
# ... wait for it to finish ...
ss -tln | sort > after.txt
diff before.txt after.txt      # expected: empty
```

RPC + serial + monitor all live in Unix sockets under `<runtime_dir>/`.

## 5. Inspect VFS RPC traffic

Each mount gets a Unix socket at `<runtime_dir>/rpc<N>.sock`. Tee the
postcard frames with:

```bash
# Move the original socket aside, then forward bytes both ways,
# copying them to stdout for human inspection.
mv /tmp/tokimo-.../rpc0.sock /tmp/tokimo-.../rpc0.sock.real
socat -v UNIX-LISTEN:/tmp/tokimo-.../rpc0.sock,fork \
          UNIX-CONNECT:/tmp/tokimo-.../rpc0.sock.real
```

(Do this before the guest's tokimo-agent connects, so you can see the
initial handshake.)

## 6. Guest-side logs

`tokimo-init` writes its own diagnostics to stdout (→ serial.log) and
per-agent logs to `/var/log/tokimo-agent-<tag>.log` **inside** the
guest. The rootfs squashfs is read-only, but init mounts a tmpfs for
`/var/log`, so those paths work at runtime.

If a FUSE mount fails, init dumps the agent log to the serial console
prefixed with `  agent:`, plus a listing of `/sys/class/virtio-ports/`
so you can see whether the port even appeared.

## 7. QEMU stderr

Boot-time QEMU errors (bad chardev path, WHPX unavailable on Windows,
KVM locked by another VM, …) land in `<runtime_dir>/qemu.stderr.log`.

## 8. Rebuilding the guest image incrementally

Editing only the init script? You don't need a full `mmdebstrap` run
(~14 min). Fast path (~30 s):

```bash
sudo unsquashfs -d /tmp/edit img/rootfs.squashfs
sudo $EDITOR /tmp/edit/sbin/tokimo-init
sudo mksquashfs /tmp/edit img/rootfs.squashfs -noappend -comp zstd
sudo chown $USER img/rootfs.squashfs
```

Then re-run any example. No reboot of anything else required.

## 9. Common issues

| Symptom                                         | Likely cause / fix                                                  |
| ----------------------------------------------- | ------------------------------------------------------------------- |
| `could not open /dev/kvm: Permission denied`    | `sudo usermod -aG kvm $USER` then re-login                          |
| `mount FAILED` in serial log                    | Check `/var/log/tokimo-agent-<tag>.log` lines that init dumped      |
| REPL hangs at boot                              | `qemu.stderr.log` likely shows a KVM issue; try `-accel tcg` via env|
| Demos exit 0 but produce no output              | serial.log flushed after poweroff; re-run, `tail -F` in parallel    |
| Windows: `WHPX not available`                   | Enable *Windows Hypervisor Platform* optional feature and reboot    |
