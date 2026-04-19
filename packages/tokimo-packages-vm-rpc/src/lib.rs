//! RPC wire protocol for [`tokimo_packages_vm_core::TokimoVfs`].
//!
//! Transport is a bidirectional byte stream — on Linux the host side is
//! a Unix-domain socket exposed to the guest via `virtio-serial`, and the
//! guest side is `/dev/virtio-ports/tokimo.rpc.<tag>`. On Windows the host
//! side is a named pipe. macOS uses a Unix socket pair attached to a
//! `VZVirtioConsolePortConfiguration`. In every case both sides see an
//! `AsyncRead + AsyncWrite` half-duplex pair, so the protocol code is
//! transport-agnostic.

pub mod proto;
pub mod frame;
pub mod server;
pub mod client;

pub use proto::{Request, Response, PROTOCOL_VERSION};
pub use client::Client;
pub use server::serve_stream;
