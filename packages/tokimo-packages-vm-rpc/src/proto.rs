//! Wire protocol mirroring `TokimoVfs`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokimo_packages_vm_core::{DirEntry, FileAttr, VfsError};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Hello { protocol_version: u32 },
    Stat { path: PathBuf },
    List { path: PathBuf },
    Read { path: PathBuf, offset: u64, len: u32 },
    Write { path: PathBuf, offset: u64, data: Vec<u8> },
    Create { path: PathBuf, mode: u32 },
    Mkdir { path: PathBuf, mode: u32 },
    Remove { path: PathBuf },
    Rmdir { path: PathBuf },
    Rename { from: PathBuf, to: PathBuf },
    Truncate { path: PathBuf, size: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Hello { protocol_version: u32 },
    Stat(Result<FileAttr, VfsError>),
    List(Result<Vec<DirEntry>, VfsError>),
    Read(Result<Vec<u8>, VfsError>),
    Write(Result<u32, VfsError>),
    Create(Result<FileAttr, VfsError>),
    Mkdir(Result<FileAttr, VfsError>),
    Remove(Result<(), VfsError>),
    Rmdir(Result<(), VfsError>),
    Rename(Result<(), VfsError>),
    Truncate(Result<(), VfsError>),
}
