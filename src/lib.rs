//! record-ui library: injectable ports, config, and Recorder state machine.
//!
//! The binary (`main.rs`) wires CLI/IPC/GUI over these modules in later slices.

pub mod config;
pub mod ports;
pub mod recorder;

pub use config::Config;
pub use recorder::{CommandResult, MachineCode, Recorder, State, Status};
