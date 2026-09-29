//! The event stream: one JSON object per line on stdout, the interface
//! other programs read. Add fields; never change one.

use std::io::Write;

use serde::Serialize;

use crate::provider::ProviderError;
use crate::run::Limits;
use crate::tools::ToolStatus;
use crate::tools::shell::Sandbox;

/// An event with the run-relative time it happened at.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Record {
    #[serde(flatten)]
    pub event: Event,
    pub t_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Start {
        version: String,
        profile_hash: String,
        model: String,
        tools: Vec<String>,
        /// The shell's sandbox; `None` when no shell is offered.
        sandbox: Option<Sandbox>,
        limits: Limits,
    },
    /// Written as a request leaves, so a slow call shows as in flight.
    ModelRequest {
        step: u32,
        messages: u32,
    },
    /// The reply to the `model_request` of the same step.
    ModelCall {
        step: u32,
        generation_id: Option<String>,
        model: Option<String>,
        provider: Option<String>,
        input_tokens: u32,
        output_tokens: u32,
        cached_tokens: Option<u32>,
        /// Counted in `output_tokens`, not on top of them.
        reasoning_tokens: Option<u32>,
        cost_nano_usd: Option<u64>,
        duration_ms: u64,
        finish_reason: Option<String>,
    },
    ToolCall {
        step: u32,
        tool: String,
        arguments: String,
        status: ToolStatus,
        /// The result's size before any cut.
        result_bytes: u64,
        cut: bool,
        duration_ms: u64,
    },
    Answer {
        text: String,
    },
    Exit {
        #[serde(flatten)]
        outcome: Outcome,
        totals: Totals,
    },
}

/// Why a run ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Outcome {
    /// The model answered without calling a tool.
    Done,
    Limit {
        which: LimitKind,
    },
    Error {
        error: RunError,
    },
}

/// The limits, named as the profile names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    Steps,
    WallS,
    CostUsd,
    Tokens,
    ContextTokens,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunError {
    Provider(ProviderError),
    /// A cost limit is set but a reply reported no cost, so the limit
    /// could no longer be enforced.
    CostUnreported,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    pub steps: u32,
    pub tool_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// `None` once any reply reported no cost: a partial sum would
    /// understate the run.
    pub cost_nano_usd: Option<u64>,
}

pub trait Sink {
    fn emit(&mut self, record: Record);
}

/// Writes each record as one JSON line and flushes it at once, so a
/// caller watches the run live.
pub struct JsonLines<W: Write> {
    out: W,
}

impl<W: Write> JsonLines<W> {
    pub fn new(out: W) -> Self {
        Self { out }
    }
}

impl<W: Write> Sink for JsonLines<W> {
    fn emit(&mut self, record: Record) {
        // A run whose events can't be written has no record, so it
        // can't go on: crash, and the missing exit line voids it.
        serde_json::to_writer(&mut self.out, &record).expect("event serializes");
        self.out.write_all(b"\n").expect("event stream is writable");
        self.out.flush().expect("event stream is writable");
    }
}
