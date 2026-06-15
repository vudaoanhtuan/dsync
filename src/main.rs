//! Thin entry point: parse args, dispatch, render errors and map them to exit codes. The hidden
//! `--server <root>` flag enters remote-agent mode (server.rs). See specs/cli.md.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser};

use dsync::cli::{self, Cli};
use dsync::server;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    let raw: Vec<String> = std::env::args().collect();

    // Remote-agent mode is intercepted before clap so the trailing root path (passed on the SSH
    // exec line as `dsync --server <root>`) is not misparsed as a subcommand.
    if let Some(pos) = raw.iter().position(|a| a == "--server") {
        let root = raw.get(pos + 1).cloned().unwrap_or_else(|| ".".to_string());
        return match server::run(PathBuf::from(root)).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("dsync --server: {e}");
                ExitCode::from(1)
            }
        };
    }

    let cli = Cli::parse();
    let Some(command) = cli.command else {
        // No subcommand: show help and exit with a usage code.
        let _ = Cli::command().print_help();
        println!();
        return ExitCode::from(2);
    };

    match cli::dispatch(command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(cli::exit_code(&e) as u8)
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("DSYNC_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
