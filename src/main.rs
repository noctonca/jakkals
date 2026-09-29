//! Jakkals: a small coding agent whose every model call, tool call and
//! limit is written down. See docs/ARCHITECTURE.md for the design.
//!
//! Scaffold only: `run` parses its arguments and stops.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one task to its end and write the run's events to stdout.
    Run {
        /// The profile: system prompt, tools, limits (TOML).
        #[arg(long)]
        profile: PathBuf,
        /// The model to call, as the provider names it.
        #[arg(long)]
        model: String,
        /// The directory the agent works in; its tools can't leave it.
        #[arg(long)]
        cwd: PathBuf,
        /// The task.
        #[arg(long)]
        prompt: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Run { .. } => {
            eprintln!("jakkals: `run` is not implemented yet");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
