//! record-ui — entry point (CLI / server / GUI wired in later slices).

mod cli;
mod client;
mod server;
mod ui;

use record_ui::Config;

fn main() {
    // Scaffold: real CLI dispatch lands in later slices.
    // Prefer with_home / load over bare Default (relative path footgun).
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    let _cfg = Config::with_home(&home, None);
}

#[cfg(test)]
mod tests {
    #[test]
    fn scaffold_smoke() {
        assert_eq!(2 + 2, 4);
    }
}
