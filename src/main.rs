//! Jakkals: a small coding agent whose every model call, tool call and
//! limit is written down. See docs/ARCHITECTURE.md for the design.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};

use jakkals::clock::MonotonicClock;
use jakkals::events::{JsonLines, Outcome};
use jakkals::profile::{self, Profile};
use jakkals::provider::http::HttpProvider;
use jakkals::run::{Task, run};
use jakkals::tools::Toolbox;
use jakkals::tools::local::LocalTools;
use jakkals::tools::mcp::McpServer;
use jakkals::transcript::{self, JsonLinesTranscript};

/// The process's exit status, one per way a run can end, so a caller
/// can branch without reading the events. Documented in ARCHITECTURE.md.
const EXIT_DONE: u8 = 0;
/// The run never started: a bad profile, key or directory, a transcript
/// file that couldn't be created, or an MCP server that couldn't be set
/// up. clap uses the same status for bad
/// arguments. No events are written.
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
        /// Also write the conversation to a new file (mode 600), one JSON
        /// line per message. It holds everything the model saw. Without a
        /// path, it goes in $XDG_DATA_HOME/jakkals/transcripts (default
        /// ~/.local/share/jakkals/transcripts), named for the start time.
        #[arg(long, value_name = "PATH")]
        transcript: Option<Option<PathBuf>>,
        /// Include each reply's reasoning text in the transcript.
        #[arg(long, requires = "transcript")]
        transcript_reasoning: bool,
    },
}

/// A transcript the run was asked for.
struct TranscriptRequest<'a> {
    /// `None` for the default folder and a generated name.
    path: Option<&'a Path>,
    reasoning: bool,
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
            transcript,
            transcript_reasoning,
        } => {
            let transcript = transcript.as_ref().map(|path| TranscriptRequest {
                path: path.as_deref(),
                reasoning: transcript_reasoning,
            });
            match start(&profile, &model, &cwd, &prompt, transcript).await {
                Ok(outcome) => ExitCode::from(match outcome {
                    Outcome::Done => EXIT_DONE,
                    Outcome::Limit { .. } => EXIT_LIMIT,
                    Outcome::Error { .. } => EXIT_ERROR,
                }),
                Err(message) => {
                    eprintln!("jakkals: {message}");
                    ExitCode::from(EXIT_SETUP)
                }
            }
        }
    }
}

/// Everything a run needs is checked before its first event, so a run
/// that starts has only the loop's ways to end.
async fn start(
    profile: &Path,
    model: &str,
    cwd: &Path,
    prompt: &str,
    transcript: Option<TranscriptRequest<'_>>,
) -> Result<Outcome, String> {
    let profile = Profile::read(profile).map_err(|error| error.to_string())?;
    if !cwd.is_dir() {
        return Err(format!("--cwd {} is not a directory", cwd.display()));
    }
    let local = LocalTools::new(
        cwd,
        &profile.local_tools,
        profile.shell.as_ref(),
        std::env::var_os("PATH"),
    )
    .map_err(|error| error.to_string())?;
    // Every key is read before any server is reached, so a missing one
    // costs no connection.
    let keys = profile
        .mcp
        .iter()
        .map(|server| profile::mcp_key(server, |variable| std::env::var(variable).ok()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let config = profile
        .http_config(|variable| std::env::var(variable).ok())
        .map_err(|error| error.to_string())?;
    let mut provider =
        HttpProvider::new(config).map_err(|error| format!("the provider: {error}"))?;
    let mut servers = Vec::with_capacity(profile.mcp.len());
    for (settings, key) in profile.mcp.iter().zip(&keys) {
        servers.push(
            McpServer::connect(settings, key.as_deref())
                .await
                .map_err(|error| error.to_string())?,
        );
    }
    let mut tools = Toolbox::new(local, servers);
    // Created last, so a run that fails setup leaves no empty file whose
    // path the next attempt would be refused.
    let (mut transcript, transcript_path) = match transcript {
        None => (None, None),
        Some(request) => {
            let path = transcript_path(request.path)?;
            let file = transcript::create(&path)
                .map_err(|error| format!("--transcript {}: {error}", path.display()))?;
            (
                Some(JsonLinesTranscript::new(file, request.reasoning)),
                Some(path.to_string_lossy().into_owned()),
            )
        }
    };

    let task = Task {
        system_prompt: &profile.system_prompt,
        prompt,
        model,
        profile_hash: &profile.hash,
        transcript: transcript_path.as_deref(),
    };
    let mut sink = JsonLines::new(std::io::stdout().lock());
    Ok(run(
        &task,
        &profile.limits,
        &mut provider,
        &mut tools,
        &MonotonicClock::start(),
        &mut sink,
        &mut transcript,
    )
    .await)
}

/// The transcript's absolute path: the one given, or a new name in the
/// default folder, which is made if missing.
fn transcript_path(given: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = given {
        return std::path::absolute(path)
            .map_err(|error| format!("--transcript {}: {error}", path.display()));
    }
    let folder =
        transcript::default_folder(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
            .ok_or("--transcript: neither XDG_DATA_HOME nor HOME is an absolute path")?;
    transcript::create_folder(&folder)
        .map_err(|error| format!("--transcript {}: {error}", folder.display()))?;
    let unix_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs();
    Ok(folder.join(transcript::file_name(unix_s, std::process::id())))
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
