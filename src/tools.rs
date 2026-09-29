//! The tools, as the loop sees them. Local tools and MCP servers live at
//! the edge behind this trait; [`Toolbox`] offers both.

use serde::Serialize;

use crate::conversation::{ToolCall, ToolSpec};
use crate::tools::local::LocalTools;
use crate::tools::mcp::{McpServer, McpServerRecord};

pub mod local;
pub mod mcp;
pub mod shell;

// See the note on `Provider`: generic only, no `Send` needed.
#[allow(async_fn_in_trait)]
pub trait Tools {
    /// The tools offered to the model, in a fixed order.
    fn specs(&self) -> &[ToolSpec];

    /// How shell commands are confined, when a shell is offered.
    fn sandbox(&self) -> Option<shell::Sandbox>;

    /// The MCP servers the tools come from, in the order offered.
    fn servers(&self) -> Vec<McpServerRecord>;

    /// Runs one call to a tool named in `specs`, returning within
    /// `deadline_ms`. Every failure is an outcome, not an error: it goes
    /// back to the model.
    async fn call(&mut self, call: &ToolCall, deadline_ms: u64) -> ToolOutcome;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    /// The tool ran; its output.
    Ok(String),
    /// The tool ran and failed; why, and the error as the model will
    /// read it.
    Failed(Failure, String),
    /// The tool refused to run the call (an allowlist, a path outside the
    /// working directory); why, and the reason as the model will read it.
    Refused(Refusal, String),
}

impl ToolOutcome {
    pub fn status(&self) -> ToolStatus {
        match self {
            ToolOutcome::Ok(_) => ToolStatus::Ok,
            ToolOutcome::Failed(..) => ToolStatus::Failed,
            ToolOutcome::Refused(..) => ToolStatus::Refused,
        }
    }

    pub fn cause(&self) -> Option<Cause> {
        match self {
            ToolOutcome::Ok(_) => None,
            ToolOutcome::Failed(failure, _) => Some(Cause::Failed(*failure)),
            ToolOutcome::Refused(refusal, _) => Some(Cause::Refused(*refusal)),
        }
    }

    pub fn into_text(self) -> String {
        match self {
            ToolOutcome::Ok(text)
            | ToolOutcome::Failed(_, text)
            | ToolOutcome::Refused(_, text) => text,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Failed,
    Refused,
}

/// Why a call that ran failed, set where it fails: never read back out
/// of the text the model gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Failure {
    Arguments,
    NotFound,
    /// `read` of a directory, `list` of a file.
    WrongType,
    TooLarge,
    NotText,
    /// Any other file-system error.
    Io,
    Spawn,
    ExitStatus {
        status: i32,
    },
    /// A signal Jakkals didn't send.
    Signal {
        signal: i32,
    },
    Lost,
    /// The tool's own time: `shell_timeout_s`, `call_timeout_s`.
    Timeout,
    /// The run's deadline, reached before the tool's own time.
    Deadline,
    IsError,
    McpError {
        code: i32,
    },
    InputRequired,
    Task,
    UnexpectedReply,
    Connection,
}

/// Why a call never ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Refusal {
    NotOffered,
    OutsideCwd,
    NotAllowed,
    ShellSyntax,
}

/// A `tool_call` event's `cause`: the kinds of both are distinct, so
/// the event needs no wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Cause {
    Failed(Failure),
    Refused(Refusal),
}

/// Why a call ended without a result, and the text the model will read:
/// the failing half of a tool's `Result`, turned into a [`ToolOutcome`].
pub(crate) enum Stop {
    Failed(Failure, String),
    Refused(Refusal, String),
}

impl From<Result<String, Stop>> for ToolOutcome {
    fn from(result: Result<String, Stop>) -> Self {
        match result {
            Ok(text) => ToolOutcome::Ok(text),
            Err(Stop::Failed(failure, text)) => ToolOutcome::Failed(failure, text),
            Err(Stop::Refused(refusal, text)) => ToolOutcome::Refused(refusal, text),
        }
    }
}

/// The tools a run offers: the local ones first, then each MCP server's.
pub struct Toolbox {
    local: LocalTools,
    servers: Vec<McpServer>,
    specs: Vec<ToolSpec>,
}

impl Toolbox {
    /// `servers` in the order the profile gives them.
    pub fn new(local: LocalTools, servers: Vec<McpServer>) -> Self {
        let specs: Vec<ToolSpec> = local
            .specs()
            .iter()
            .chain(servers.iter().flat_map(|server| server.specs()))
            .cloned()
            .collect();
        // The profile keeps names apart; a clash here is a bug.
        for (index, spec) in specs.iter().enumerate() {
            assert!(
                specs[..index]
                    .iter()
                    .all(|earlier| earlier.name != spec.name),
                "tool names are unique"
            );
        }
        Self {
            local,
            servers,
            specs,
        }
    }
}

impl Tools for Toolbox {
    fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    fn sandbox(&self) -> Option<shell::Sandbox> {
        self.local.sandbox()
    }

    fn servers(&self) -> Vec<McpServerRecord> {
        self.servers
            .iter()
            .map(|server| server.record().clone())
            .collect()
    }

    async fn call(&mut self, call: &ToolCall, deadline_ms: u64) -> ToolOutcome {
        match self.servers.iter().find(|server| server.offers(&call.name)) {
            Some(server) => server.call(&call.name, &call.arguments, deadline_ms).await,
            None => self.local.call(call, deadline_ms).await,
        }
    }
}
