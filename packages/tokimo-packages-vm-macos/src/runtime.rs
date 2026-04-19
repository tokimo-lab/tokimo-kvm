//! Runtime directory layout for the macOS backend.

use std::path::PathBuf;

pub struct Runtime {
    pub root: PathBuf,
    pub serial_log: PathBuf,
}

impl Runtime {
    pub fn new(root: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        let serial_log = root.join("serial.log");
        let _ = std::fs::File::create(&serial_log)?;
        Ok(Self { root, serial_log })
    }
}

pub fn default_runtime_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tokimo-{name}-{}", std::process::id()))
}
