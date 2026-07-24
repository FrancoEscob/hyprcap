//! record-ui library: ports, config, Recorder, IPC server/client.
//!
//! The binary (`main.rs`) wires CLI over these modules. GTK/adw lives only on
//! the future `gui` path (slice 04) and is not initialized from library code.

pub mod client;
pub mod config;
pub mod ipc;
pub mod ports;
pub mod recorder;
pub mod server;
pub mod sys;

pub use config::Config;
pub use ipc::{IpcRequest, IpcResponse, IpcStatus};
pub use recorder::{CommandResult, MachineCode, Recorder, State, Status};
pub use server::RuntimePaths;
