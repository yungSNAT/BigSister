use std::io::{ErrorKind, Write};
use std::process::ExitCode;

use clap::Parser;

#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let cli = bigsister::cli::Cli::parse();
    let out = match bigsister::run(&cli) {
        Ok(out) => out,
        Err(e) => {
            eprintln!("bigsister: {e}");
            return ExitCode::FAILURE;
        }
    };
    match std::io::stdout().lock().write_all(out.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bigsister: cannot write output: {e}");
            ExitCode::FAILURE
        }
    }
}
