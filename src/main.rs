//! record-ui — entry point (scaffold).

mod cli;
mod client;
mod config;
mod ports;
mod recorder;
mod server;
mod ui;

fn main() {
    // Scaffold: real CLI dispatch lands in later slices.
}

#[cfg(test)]
mod tests {
    #[test]
    fn scaffold_smoke() {
        assert!(true);
    }
}
