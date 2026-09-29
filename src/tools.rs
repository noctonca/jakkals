//! The tools, as the loop sees them. Local tools and MCP servers live at
//! the edge behind this trait.

use serde::Serialize;

use crate::conversation::{ToolCall, ToolSpec};

pub mod local;
pub mod shell;

// See the note on `Provider`: generic only, no `Send` needed.
#[allow(async_fn_in_trait)]
pub trait Tools {
    /// The tools offered to the model, in a fixed order.
    fn specs(&self) -> &[ToolSpec];

    /// How shell commands are confined, when a shell is offered.
    fn sandbox(&self) -> Option<shell::Sandbox>;

    /// Runs one call to a tool named in `specs`, returning within
    /// `deadline_ms`. Every failure is an outcome, not an error: it goes
    /// back to the model.
    async fn call(&mut self, call: &ToolCall, deadline_ms: u64) -> ToolOutcome;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    /// The tool ran; its output.
    Ok(String),
    /// The tool ran and failed; the error, as the model will read it.
    Failed(String),
    /// The tool refused to run the call (an allowlist, a path outside the
    /// working directory); the reason, as the model will read it.
    Refused(String),
}

impl ToolOutcome {
    pub fn status(&self) -> ToolStatus {
        match self {
            ToolOutcome::Ok(_) => ToolStatus::Ok,
            ToolOutcome::Failed(_) => ToolStatus::Failed,
            ToolOutcome::Refused(_) => ToolStatus::Refused,
        }
    }

    pub fn into_text(self) -> String {
        match self {
            ToolOutcome::Ok(text) | ToolOutcome::Failed(text) | ToolOutcome::Refused(text) => text,
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

/// Why a call ended without a result, as the model will read it: the
/// failing half of a tool's `Result`, turned into a [`ToolOutcome`].
pub(crate) enum Stop {
    Failed(String),
    Refused(String),
}

impl From<Result<String, Stop>> for ToolOutcome {
    fn from(result: Result<String, Stop>) -> Self {
        match result {
            Ok(text) => ToolOutcome::Ok(text),
            Err(Stop::Failed(text)) => ToolOutcome::Failed(text),
            Err(Stop::Refused(text)) => ToolOutcome::Refused(text),
        }
    }
}
