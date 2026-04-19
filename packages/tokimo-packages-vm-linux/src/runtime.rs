//! Runtime directory layout.

use std::path::{Path, PathBuf};

pub struct Runtime {
    pub root: PathBuf,
    pub serial_log: PathBuf,
    pub serial_sock: PathBuf,
    pub qmp_sock: PathBuf,
    pub monitor_sock: PathBuf,
}

impl Runtime {
    pub fn new(root: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        let serial_log = root.join("serial.log");
        let _ = std::fs::File::create(&serial_log)?;
        let serial_sock = root.join("serial.sock");
        let _ = std::fs::remove_file(&serial_sock);
        let qmp_sock = root.join("qmp.sock");
        let monitor_sock = root.join("monitor.sock");
        let _ = std::fs::remove_file(&qmp_sock);
        let _ = std::fs::remove_file(&monitor_sock);
        Ok(Self { root, serial_log, serial_sock, qmp_sock, monitor_sock })
    }
}

pub fn default_runtime_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tokimo-{name}-{}", std::process::id()))
}

pub fn ensure_dir(p: &Path) -> std::io::Result<()> {
    if !p.exists() { std::fs::create_dir_all(p)?; }
    Ok(())
}
