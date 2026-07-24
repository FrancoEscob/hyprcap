//! record-ui — CLI client / optional GUI view / internal `--server` daemon.
//!
//! GTK/Adwaita are initialized only on the `gui` path (`ui::run_gui`).

mod cli;
mod ui;

fn main() {
    std::process::exit(cli::run());
}
