//! Build the QEMU command line.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};

use tokimo_packages_vm_core::{NetworkSpec, Protocol};

pub const QEMU_DEFAULT: &str = "/usr/bin/qemu-system-x86_64";

/// A virtio-serial port exposed as a Unix socket on the host and as
/// `/dev/virtio-ports/<port_name>` on the guest.
pub struct SerialPort {
    pub chardev_id: String,
    pub port_name: String,
    pub socket_path: PathBuf,
}

pub struct QemuSpec<'a> {
    pub qemu_bin: &'a Path,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub kernel: &'a Path,
    pub initrd: &'a Path,
    pub rootfs: Option<&'a Path>,
    pub cmdline: &'a str,
    pub serial_ports: &'a [SerialPort],
    pub network: &'a NetworkSpec,
    pub qmp_socket: &'a Path,
    pub serial_log: &'a Path,
    /// When `Some`, serial console is exposed as a bidirectional Unix
    /// socket at this path instead of written to `serial_log`.
    pub serial_socket: Option<&'a Path>,
    pub monitor_socket: &'a Path,
    pub runtime_dir: &'a Path,
}

pub fn build_command(spec: &QemuSpec) -> Command {
    let mut cmd = Command::new(spec.qemu_bin);
    cmd.arg("-enable-kvm")
        .arg("-machine")
        .arg("q35,accel=kvm")
        .arg("-cpu")
        .arg("host")
        .arg("-smp")
        .arg(spec.vcpus.to_string())
        .arg("-m")
        .arg(format!("{}M", spec.memory_mib))
        .arg("-kernel")
        .arg(spec.kernel)
        .arg("-initrd")
        .arg(spec.initrd)
        .arg("-append")
        .arg(spec.cmdline)
        .arg("-display")
        .arg("none")
        .arg("-nodefaults")
        .arg("-no-reboot");

    // Serial console: either a Unix socket (interactive) or an
    // append-only log file (batch / default).
    if let Some(sp) = spec.serial_socket {
        cmd.arg("-chardev").arg(format!(
            "socket,id=sercon,path={},server=on,wait=off",
            sp.display()
        ));
    } else {
        cmd.arg("-chardev").arg(format!(
            "file,id=sercon,path={},append=on",
            spec.serial_log.display()
        ));
    }
    cmd.arg("-serial").arg("chardev:sercon");

    // QMP control socket (unix).
    cmd.arg("-qmp").arg(format!(
        "unix:{},server=on,wait=off",
        spec.qmp_socket.display()
    ));
    // Human monitor over a unix socket too; convenient for debug tooling.
    cmd.arg("-monitor").arg(format!(
        "unix:{},server=on,wait=off",
        spec.monitor_socket.display()
    ));

    // Rootfs (squashfs via virtio-blk).
    if let Some(rootfs) = spec.rootfs {
        cmd.arg("-drive").arg(format!(
            "file={},if=none,readonly=on,id=rootfs,format=raw",
            rootfs.display()
        ));
        cmd.arg("-device")
            .arg("virtio-blk-pci,drive=rootfs,bootindex=1");
    }

    // virtio-serial bus + one virtserialport per mount (both HostDir and Vfs).
    if !spec.serial_ports.is_empty() {
        cmd.arg("-device").arg("virtio-serial-pci,id=vser0");
        for p in spec.serial_ports {
            cmd.arg("-chardev").arg(format!(
                "socket,id={id},path={path},server=off,reconnect=1",
                id = p.chardev_id,
                path = p.socket_path.display()
            ));
            cmd.arg("-device").arg(format!(
                "virtserialport,chardev={id},bus=vser0.0,name={name}",
                id = p.chardev_id,
                name = p.port_name
            ));
        }
    }

    // Networking (user-mode NAT; port_forwards may be empty).
    if spec.network.user_mode {
        let mut netdev = String::from("user,id=net0");
        for pf in &spec.network.port_forwards {
            let proto = match pf.protocol {
                Protocol::Tcp => "tcp",
                Protocol::Udp => "udp",
            };
            netdev.push_str(&format!(
                ",hostfwd={}::{host}-:{guest}",
                proto,
                host = pf.host_port,
                guest = pf.guest_port
            ));
        }
        cmd.arg("-netdev").arg(netdev);
        cmd.arg("-device").arg("virtio-net-pci,netdev=net0");
    }

    cmd.current_dir(spec.runtime_dir);
    // Pipe stderr into a file so boot-time QEMU errors are debuggable.
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(spec.runtime_dir.join("qemu.stderr.log"))
    {
        cmd.stderr(Stdio::from(f));
    } else {
        cmd.stderr(Stdio::null());
    }
    cmd.stdout(Stdio::null());
    cmd.stdin(Stdio::null());
    cmd.kill_on_drop(true);
    cmd
}

pub async fn spawn(mut cmd: Command) -> anyhow::Result<Child> {
    tracing::info!("spawning qemu: {:?}", cmd.as_std());
    Ok(cmd.spawn()?)
}
