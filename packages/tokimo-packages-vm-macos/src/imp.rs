//! macOS Virtualization.framework backend.
//!
//! VFS mounts are wired through a `VZVirtioConsoleDeviceConfiguration`
//! exposing one `VZVirtioConsolePortConfiguration` per mount, named
//! `tokimo.rpc.<tag>`. Each port is backed by a Unix socket pair: one
//! end is handed to VZ as a `VZFileHandleSerialPortAttachment`, the
//! other end stays on the host and runs the RPC server.
//!
//! No host TCP/port is opened.

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_foundation::{NSArray, NSError, NSFileHandle, NSString, NSURL};
use objc2_virtualization::{
    VZConsoleDeviceConfiguration, VZConsolePortConfiguration,
    VZDirectorySharingDeviceConfiguration, VZDiskImageStorageDeviceAttachment,
    VZEntropyDeviceConfiguration, VZFileHandleSerialPortAttachment, VZGenericPlatformConfiguration,
    VZLinuxBootLoader, VZNATNetworkDeviceAttachment, VZNetworkDeviceAttachment,
    VZNetworkDeviceConfiguration, VZPlatformConfiguration, VZSerialPortConfiguration,
    VZStorageDeviceConfiguration, VZVirtioBlockDeviceConfiguration,
    VZVirtioConsoleDeviceConfiguration, VZVirtioConsoleDeviceSerialPortConfiguration,
    VZVirtioConsolePortConfiguration, VZVirtioEntropyDeviceConfiguration,
    VZVirtioNetworkDeviceConfiguration, VZVirtualMachine, VZVirtualMachineConfiguration,
};
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::thread;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use tokimo_packages_vm_core::{Error, ImagePaths, MountSpec, Result, SandboxConfig, TokimoVfs};
use tokimo_packages_vm_rpc as rpc;

use crate::runtime;

enum Cmd {
    Stop(oneshot::Sender<Result<()>>),
    Wait(oneshot::Sender<Result<()>>),
    Shutdown,
}

pub(crate) struct Inner {
    tx: mpsc::Sender<Cmd>,
    thread: Option<thread::JoinHandle<()>>,
    _rpc_tasks: Vec<JoinHandle<()>>,
}

/// One virtio-console port backed by a socket-pair FD handed to VZ.
struct ConsolePort {
    name: String,
    guest_fd: RawFd,
}

impl Inner {
    pub(crate) async fn start(cfg: &SandboxConfig) -> Result<(Self, PathBuf)> {
        let image = cfg
            .image
            .clone()
            .or_else(ImagePaths::from_env_or_default)
            .ok_or_else(|| Error::Config("no image (set TOKIMO_IMG_DIR or build img/)".into()))?;

        let root = cfg
            .runtime_dir
            .clone()
            .unwrap_or_else(|| runtime::default_runtime_root(&cfg.name));
        let rt = runtime::Runtime::new(root).map_err(Error::Io)?;
        let serial_log = rt.serial_log.clone();

        let mut cmd_mounts: Vec<(String, String)> = Vec::new();
        let mut rpc_tasks: Vec<JoinHandle<()>> = Vec::new();
        let mut ports: Vec<ConsolePort> = Vec::new();

        for m in &cfg.mounts {
            let tag = m.tag().to_string();
            let gp = m.guest_path().to_string();
            // HostDir is wrapped by the built-in HostFs adapter so both
            // HostDir and Vfs flow through the same virtio-console RPC
            // pipeline — no VZSingleDirectoryShare / 9p.
            let vfs: Arc<dyn TokimoVfs> = match m {
                MountSpec::HostDir {
                    host_path,
                    read_only,
                    ..
                } => Arc::new(tokimo_packages_vm_core::HostFs::new(
                    host_path.clone(),
                    *read_only,
                )),
                MountSpec::Vfs { vfs, .. } => vfs.clone(),
            };
            let (host_fd, guest_fd) = socket_pair()?;
            let host_stream =
                unsafe { std::os::unix::net::UnixStream::from_raw_fd(host_fd.into_raw_fd()) };
            host_stream.set_nonblocking(true).map_err(Error::Io)?;
            let host_tokio = UnixStream::from_std(host_stream).map_err(Error::Io)?;
            rpc_tasks.push(tokio::spawn(async move {
                let _ = rpc::serve_stream(host_tokio, vfs).await;
            }));
            ports.push(ConsolePort {
                name: format!("tokimo.rpc.{tag}"),
                guest_fd: guest_fd.into_raw_fd(),
            });
            cmd_mounts.push((tag, gp));
        }

        let cmdline = build_cmdline(&cfg.extra_cmdline, &cmd_mounts, image.rootfs.is_some());

        let serial_fd: RawFd = std::fs::OpenOptions::new()
            .write(true)
            .append(true)
            .open(&serial_log)
            .map_err(Error::Io)?
            .into_raw_fd();

        let vcpus = cfg.vcpus;
        let memory_bytes: u64 = (cfg.memory_mib as u64) * 1024 * 1024;
        let rootfs = image.rootfs.clone();
        let kernel = image.kernel.clone();
        let initrd = image.initrd.clone();
        let use_nat = cfg.network.user_mode;

        let (tx, rx) = mpsc::channel::<Cmd>();
        let (start_tx, start_rx) = oneshot::channel::<Result<()>>();

        let handle = thread::Builder::new()
            .name(format!("tokimo-macos-vm-{}", cfg.name))
            .spawn(move || {
                vm_thread_main(VmThreadArgs {
                    kernel,
                    initrd,
                    rootfs,
                    cmdline,
                    vcpus,
                    memory_bytes,
                    serial_fd,
                    use_nat,
                    ports,
                    cmd_rx: rx,
                    start_tx,
                });
            })
            .map_err(Error::Io)?;

        match start_rx.await {
            Ok(Ok(())) => Ok((
                Self {
                    tx,
                    thread: Some(handle),
                    _rpc_tasks: rpc_tasks,
                },
                serial_log,
            )),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => Err(Error::Hypervisor(
                "VM thread exited before start result".into(),
            )),
        }
    }

    pub(crate) async fn stop(mut self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Cmd::Stop(tx)).is_err() {
            return Ok(());
        }
        let res = rx.await.unwrap_or_else(|_| Ok(()));
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(h) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || h.join()).await;
        }
        res
    }

    pub(crate) async fn wait(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Cmd::Wait(tx)).is_err() {
            return Ok(());
        }
        rx.await.unwrap_or_else(|_| Ok(()))
    }
}

fn socket_pair() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    let r = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if r < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

fn build_cmdline(extra: &[String], mounts: &[(String, String)], has_rootfs: bool) -> String {
    let mut parts: Vec<String> = vec![
        "console=hvc0".into(),
        "panic=-1".into(),
        "quiet".into(),
        "loglevel=3".into(),
    ];
    if has_rootfs {
        parts.push("root=/dev/vda".into());
        parts.push("rootfstype=squashfs".into());
        parts.push("ro".into());
    }
    if !mounts.is_empty() {
        let s = mounts
            .iter()
            .map(|(t, gp)| format!("{t}:{gp}"))
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("tokimo.mounts={s}"));
    }
    let (script, rest): (Vec<&String>, Vec<&String>) =
        extra.iter().partition(|s| s.starts_with("tokimo.script="));
    parts.extend(rest.into_iter().cloned());
    parts.extend(script.into_iter().cloned());
    parts.join(" ")
}

struct VmThreadArgs {
    kernel: PathBuf,
    initrd: PathBuf,
    rootfs: Option<PathBuf>,
    cmdline: String,
    vcpus: u32,
    memory_bytes: u64,
    serial_fd: RawFd,
    use_nat: bool,
    ports: Vec<ConsolePort>,
    cmd_rx: mpsc::Receiver<Cmd>,
    start_tx: oneshot::Sender<Result<()>>,
}

fn vm_thread_main(args: VmThreadArgs) {
    let VmThreadArgs {
        kernel,
        initrd,
        rootfs,
        cmdline,
        vcpus,
        memory_bytes,
        serial_fd,
        use_nat,
        ports,
        cmd_rx,
        start_tx,
    } = args;

    let vm = match build_and_start_vm(
        &kernel,
        &initrd,
        rootfs.as_deref(),
        &cmdline,
        vcpus,
        memory_bytes,
        serial_fd,
        use_nat,
        &ports,
    ) {
        Ok(v) => {
            let _ = start_tx.send(Ok(()));
            v
        }
        Err(e) => {
            let _ = start_tx.send(Err(e));
            return;
        }
    };

    let mut stopped = false;
    let mut waiters: Vec<oneshot::Sender<Result<()>>> = Vec::new();
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Cmd::Stop(r) => {
                if stopped {
                    let _ = r.send(Ok(()));
                    continue;
                }
                let res = request_stop_blocking(&vm);
                stopped = res.is_ok();
                if stopped {
                    for w in waiters.drain(..) {
                        let _ = w.send(Ok(()));
                    }
                }
                let _ = r.send(res);
            }
            Cmd::Wait(r) => {
                if stopped {
                    let _ = r.send(Ok(()));
                } else {
                    waiters.push(r);
                }
            }
            Cmd::Shutdown => {
                if !stopped {
                    let _ = request_stop_blocking(&vm);
                }
                for w in waiters.drain(..) {
                    let _ = w.send(Ok(()));
                }
                break;
            }
        }
    }
    drop(vm);
}

fn build_and_start_vm(
    kernel: &std::path::Path,
    initrd: &std::path::Path,
    rootfs: Option<&std::path::Path>,
    cmdline: &str,
    vcpus: u32,
    memory_bytes: u64,
    serial_fd: RawFd,
    use_nat: bool,
    ports: &[ConsolePort],
) -> Result<Retained<VZVirtualMachine>> {
    unsafe {
        let kernel_url = path_to_nsurl(kernel)
            .ok_or_else(|| Error::Config(format!("kernel path: {}", kernel.display())))?;
        let initrd_url = path_to_nsurl(initrd)
            .ok_or_else(|| Error::Config(format!("initrd path: {}", initrd.display())))?;

        let boot = VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &kernel_url);
        boot.setCommandLine(&NSString::from_str(cmdline));
        boot.setInitialRamdiskURL(Some(&initrd_url));

        // Primary serial console (boot log).
        let file_handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
            NSFileHandle::alloc(),
            serial_fd,
            true,
        );
        let attachment =
            VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                VZFileHandleSerialPortAttachment::alloc(),
                None,
                Some(&file_handle),
            );
        let serial_cfg: Retained<VZVirtioConsoleDeviceSerialPortConfiguration> =
            VZVirtioConsoleDeviceSerialPortConfiguration::new();
        {
            let s: &VZSerialPortConfiguration = &serial_cfg;
            s.setAttachment(Some(&attachment));
        }
        let serial_ports: Retained<NSArray<VZSerialPortConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(serial_cfg)]);

        // Host directory shares are now served via virtio-console RPC
        // (HostFs adapter), so no VZDirectorySharingDevices are needed.
        let fs_array: Retained<NSArray<VZDirectorySharingDeviceConfiguration>> =
            NSArray::from_retained_slice(&[]);

        // VFS virtio-console ports.
        let mut console_devices: Vec<Retained<VZConsoleDeviceConfiguration>> = Vec::new();
        if !ports.is_empty() {
            let console_dev: Retained<VZVirtioConsoleDeviceConfiguration> =
                VZVirtioConsoleDeviceConfiguration::new();
            let port_array = console_dev.ports();
            for (i, p) in ports.iter().enumerate() {
                let guest_fh = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                    NSFileHandle::alloc(),
                    p.guest_fd,
                    true,
                );
                let attach = VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                    VZFileHandleSerialPortAttachment::alloc(),
                    Some(&guest_fh), Some(&guest_fh));
                let port_cfg: Retained<VZVirtioConsolePortConfiguration> =
                    VZVirtioConsolePortConfiguration::new();
                port_cfg.setName(Some(&NSString::from_str(&p.name)));
                {
                    let s: &VZConsolePortConfiguration = &port_cfg;
                    s.setAttachment(Some(&Retained::into_super(Retained::clone(&attach))));
                }
                port_array.setObject_atIndexedSubscript(Some(&port_cfg), i);
            }
            console_devices.push(Retained::into_super(console_dev));
        }
        let console_array: Retained<NSArray<VZConsoleDeviceConfiguration>> =
            NSArray::from_retained_slice(&console_devices);

        // Entropy.
        let entropy: Retained<VZVirtioEntropyDeviceConfiguration> =
            VZVirtioEntropyDeviceConfiguration::new();
        let entropy_array: Retained<NSArray<VZEntropyDeviceConfiguration>> =
            NSArray::from_retained_slice(&[Retained::into_super(entropy)]);

        // Platform.
        let platform: Retained<VZGenericPlatformConfiguration> =
            VZGenericPlatformConfiguration::new();
        let platform_super: Retained<VZPlatformConfiguration> = Retained::into_super(platform);

        let config = VZVirtualMachineConfiguration::new();
        config.setCPUCount(vcpus as usize);
        config.setMemorySize(memory_bytes);
        let boot_super: Retained<objc2_virtualization::VZBootLoader> = Retained::into_super(boot);
        config.setBootLoader(Some(&boot_super));
        config.setPlatform(&platform_super);
        config.setSerialPorts(&serial_ports);
        config.setDirectorySharingDevices(&fs_array);
        config.setEntropyDevices(&entropy_array);
        config.setConsoleDevices(&console_array);

        if let Some(rootfs_path) = rootfs {
            let rootfs_url = path_to_nsurl(rootfs_path)
                .ok_or_else(|| Error::Config(format!("rootfs: {}", rootfs_path.display())))?;
            let attach = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &rootfs_url,
                true,
            )
            .map_err(|e| Error::Hypervisor(format!("rootfs attach: {}", ns_error_to_string(&e))))?;
            let blk = VZVirtioBlockDeviceConfiguration::initWithAttachment(
                VZVirtioBlockDeviceConfiguration::alloc(),
                &Retained::into_super(attach),
            );
            let arr: Retained<NSArray<VZStorageDeviceConfiguration>> =
                NSArray::from_retained_slice(&[Retained::into_super(blk)]);
            config.setStorageDevices(&arr);
        }

        if use_nat {
            let nat = VZNATNetworkDeviceAttachment::new();
            let nat_super: Retained<VZNetworkDeviceAttachment> = Retained::into_super(nat);
            let net = VZVirtioNetworkDeviceConfiguration::new();
            net.setAttachment(Some(&nat_super));
            let net_super: Retained<VZNetworkDeviceConfiguration> = Retained::into_super(net);
            let arr: Retained<NSArray<VZNetworkDeviceConfiguration>> =
                NSArray::from_retained_slice(&[net_super]);
            config.setNetworkDevices(&arr);
        }

        config
            .validateWithError()
            .map_err(|e| Error::Config(format!("VZ config invalid: {}", ns_error_to_string(&e))))?;

        let queue = DispatchQueue::new("io.tokimo.vm", DispatchQueueAttr::SERIAL);
        let vm = VZVirtualMachine::initWithConfiguration_queue(
            VZVirtualMachine::alloc(),
            &config,
            &queue,
        );

        let (s_tx, s_rx) = mpsc::sync_channel::<std::result::Result<(), String>>(1);
        let s_tx_block = s_tx.clone();
        let block = RcBlock::new(move |err: *mut NSError| {
            let res = if err.is_null() {
                Ok(())
            } else {
                Err(ns_error_to_string(&*err))
            };
            let _ = s_tx_block.send(res);
        });
        vm.startWithCompletionHandler(&block);
        drop(s_tx);

        match s_rx.recv() {
            Ok(Ok(())) => Ok(vm),
            Ok(Err(m)) => Err(Error::Hypervisor(format!("VM start: {m}"))),
            Err(_) => Err(Error::Hypervisor("VM start completion dropped".into())),
        }
    }
}

fn request_stop_blocking(vm: &Retained<VZVirtualMachine>) -> Result<()> {
    unsafe {
        if vm.requestStopWithError().is_ok() {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        let (tx, rx) = mpsc::sync_channel::<std::result::Result<(), String>>(1);
        let tx_block = tx.clone();
        let block = RcBlock::new(move |err: *mut NSError| {
            let res = if err.is_null() {
                Ok(())
            } else {
                Err(ns_error_to_string(&*err))
            };
            let _ = tx_block.send(res);
        });
        vm.stopWithCompletionHandler(&block);
        drop(tx);
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(m)) => Err(Error::Hypervisor(format!("VM stop: {m}"))),
            Err(_) => Err(Error::Hypervisor("VM stop timed out".into())),
        }
    }
}

unsafe fn path_to_nsurl(path: &std::path::Path) -> Option<Retained<NSURL>> {
    let s = path.to_str()?;
    Some(NSURL::fileURLWithPath(&NSString::from_str(s)))
}

fn ns_error_to_string(err: &NSError) -> String {
    err.localizedDescription().to_string()
}

// Silence unused warning for the Arc import.
#[allow(dead_code)]
fn _touch_arc(_: Arc<dyn TokimoVfs>) {}
