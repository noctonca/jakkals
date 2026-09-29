//! The transcript: the conversation as the model was sent it and wrote
//! it, one JSON object per line, for a caller that asked for it. The
//! events say what happened; this says what was said.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde::Serialize;

use crate::conversation::ToolCall;

/// One line of the transcript, with the step it belongs to (0 before
/// the first model call) and the run-relative time it happened at.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Line<'a> {
    #[serde(flatten)]
    pub entry: Entry<'a>,
    pub step: u32,
    pub t_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Entry<'a> {
    System {
        text: &'a str,
    },
    User {
        text: &'a str,
    },
    /// One model reply, including the one that ends the run.
    Assistant {
        text: Option<&'a str>,
        tool_calls: &'a [ToolCall],
        /// The outer `None` leaves the field out: the transcript doesn't
        /// record reasoning. `Some(None)` records that there was none.
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<Option<&'a str>>,
    },
    /// A tool's result, as the model got it: cut and marked.
    Tool {
        call_id: &'a str,
        tool: &'a str,
        text: &'a str,
    },
}

pub trait Transcript {
    fn record(&mut self, line: Line<'_>);
}

/// No transcript: the run's default.
pub struct NoTranscript;

impl Transcript for NoTranscript {
    fn record(&mut self, _line: Line<'_>) {}
}

impl<T: Transcript> Transcript for Option<T> {
    fn record(&mut self, line: Line<'_>) {
        if let Some(transcript) = self {
            transcript.record(line);
        }
    }
}

/// Owner read and write only: a transcript holds everything the model
/// saw, from file contents to MCP results.
const FILE_MODE: u32 = 0o600;

/// Creates the transcript file at `path`. A path that already exists is
/// refused, so no run writes over another's record or into a file
/// someone else set up with other permissions.
pub fn create(path: &Path) -> std::io::Result<BufWriter<File>> {
    let file = File::options()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(path)?;
    Ok(BufWriter::new(file))
}

/// Writes each line as one JSON object and flushes it at once, so a run
/// killed part-way leaves its transcript up to that point.
pub struct JsonLinesTranscript<W: Write> {
    out: W,
    /// Whether a reply's reasoning text is kept (`--transcript-reasoning`).
    reasoning: bool,
}

impl<W: Write> JsonLinesTranscript<W> {
    pub fn new(out: W, reasoning: bool) -> Self {
        Self { out, reasoning }
    }
}

impl<W: Write> Transcript for JsonLinesTranscript<W> {
    fn record(&mut self, mut line: Line<'_>) {
        if let Entry::Assistant { reasoning, .. } = &mut line.entry {
            if self.reasoning {
                assert!(
                    reasoning.is_some(),
                    "the loop passes every reply's reasoning"
                );
            } else {
                *reasoning = None;
            }
        }
        // A transcript with a gap would mislead whoever reads it, so a
        // line that can't be written ends the run, as an event does.
        serde_json::to_writer(&mut self.out, &line).expect("transcript line serializes");
        self.out
            .write_all(b"\n")
            .expect("transcript file is writable");
        self.out.flush().expect("transcript file is writable");
    }
}

#[cfg(test)]
mod tests;
