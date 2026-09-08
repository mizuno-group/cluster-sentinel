//! The `sentinel` binary.
//!
//! Production ships exactly one executable; controller, agent and every CLI
//! verb are subcommands of it (IMPLEMENTATION.md §1).

use clap::Parser;

use sentinel::cli::{self, Cli};
use sentinel::telemetry;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    telemetry::init(cli.verbose, cli.log_format());

    match cli::run(cli).await {
        Ok(code) => std::process::ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            // Report the whole chain: "cannot read config file" without the
            // underlying ENOENT is not actionable.
            eprintln!("error: {error:#}");
            std::process::ExitCode::from(1)
        }
    }
}
