//! GTK4 + libadwaita GUI view (stub for slice 04).
//!
//! Intentionally does **not** depend on or initialize GTK/adw so CLI paths
//! stay free of GUI toolkit init. Slice 04 will replace this with a real window
//! that attaches as a client view over the IPC socket.

/// Placeholder so the module is non-empty for later wiring.
#[allow(dead_code)]
pub fn gui_stub_message() -> &'static str {
    "GUI is implemented in slice 04"
}
