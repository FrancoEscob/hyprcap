//! hyprcap library: ports, config, Recorder, IPC server/client.
//!
//! The binary (`main.rs`) wires CLI and the optional GUI view. GTK/adw live only
//! in the binary `ui` module and are never initialized from library code.

pub mod audio;
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
