//! Jakkals: a small coding agent whose every model call, tool call and
//! limit is written down. See docs/ARCHITECTURE.md for the design.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use jakkals::clock::MonotonicClock;
use jakkals::events::{JsonLines, Outcome};
use jakkals::profile::Profile;
use jakkals::provider::http::HttpProvider;
use jakkals::run::{Task, run};
use jakkals::tools::local::LocalTools;

/// The process's exit status, one per way a run can end, so a caller
/// can branch without reading the events. Documented in ARCHITECTURE.md.
const EXIT_DONE: u8 = 0;
/// The run never started: a bad profile, key or directory. clap uses
/// the same status for bad arguments. No events are written.
const EXIT_SETUP: u8 = 2;
const EXIT_LIMIT: u8 = 3;
const EXIT_ERROR: u8 = 4;

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

// One thread: the loop is sequential, and nothing it awaits needs a
// second one.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            profile,
            model,
            cwd,
            prompt,
        } => match start(&profile, &model, &cwd, &prompt).await {
            Ok(outcome) => ExitCode::from(match outcome {
                Outcome::Done => EXIT_DONE,
                Outcome::Limit { .. } => EXIT_LIMIT,
                Outcome::Error { .. } => EXIT_ERROR,
            }),
            Err(message) => {
                eprintln!("jakkals: {message}");
                ExitCode::from(EXIT_SETUP)
            }
        },
    }
}

/// Everything a run needs is checked before its first event, so a run
/// that starts has only the loop's ways to end.
async fn start(profile: &Path, model: &str, cwd: &Path, prompt: &str) -> Result<Outcome, String> {
    let profile = Profile::read(profile).map_err(|error| error.to_string())?;
    if !cwd.is_dir() {
        return Err(format!("--cwd {} is not a directory", cwd.display()));
    }
    let mut tools = LocalTools::new(cwd, &profile.local_tools)
        .map_err(|error| format!("--cwd {}: {error}", cwd.display()))?;
    let config = profile
        .http_config(|variable| std::env::var(variable).ok())
        .map_err(|error| error.to_string())?;
    let mut provider =
        HttpProvider::new(config).map_err(|error| format!("the provider: {error}"))?;

    let task = Task {
        system_prompt: &profile.system_prompt,
        prompt,
        model,
        profile_hash: &profile.hash,
    };
    let mut sink = JsonLines::new(std::io::stdout().lock());
    Ok(run(
        &task,
        &profile.limits,
        &mut provider,
        &mut tools,
        &MonotonicClock::start(),
        &mut sink,
    )
    .await)
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
