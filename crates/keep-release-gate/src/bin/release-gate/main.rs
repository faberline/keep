//! The `release-gate` binary: one entry point for the release checks.

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (code, stdout, stderr) = keep_release_gate::command::run(&args);
    let _ = std::io::stdout().write_all(stdout.as_bytes());
    let _ = std::io::stderr().write_all(stderr.as_bytes());
    std::process::exit(code);
}
