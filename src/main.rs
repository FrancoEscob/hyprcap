//! record-ui — CLI client / optional GUI stub / internal `--server` daemon.

mod cli;
mod ui;

fn main() {
    std::process::exit(cli::run());
}
