//! The transcript: the conversation as the model was sent it and wrote
//! it, one JSON object per line, for a caller that asked for it. The
//! events say what happened; this says what was said.

use std::ffi::OsString;
use std::fs::{DirBuilder, File};
use std::io::{BufWriter, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

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

/// Owner only, for the folders a bare `--transcript` makes, for the
/// same reason.
const FOLDER_MODE: u32 = 0o700;

/// Where a bare `--transcript` writes: `$XDG_DATA_HOME/jakkals/transcripts`,
/// or `~/.local/share/jakkals/transcripts` when that variable is unset
/// or relative, which the XDG spec says to ignore. `None` without a
/// home either.
pub fn default_folder(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let data = xdg_data_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/share"))
        })?;
    Some(data.join("jakkals/transcripts"))
}

/// Makes the default folder and any parents missing, owner only.
pub fn create_folder(path: &Path) -> std::io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(FOLDER_MODE)
        .create(path)
}

/// A generated transcript's name: the run's start in UTC and the process
/// id, `2026-01-31T14-05-09Z-4242.jsonl`. Sorts by time, is unique on
/// one machine, and has no colon, which some file systems refuse.
pub fn file_name(unix_s: u64, pid: u32) -> String {
    let (year, month, day) = civil_date(unix_s / SECONDS_PER_DAY);
    let second_of_day = unix_s % SECONDS_PER_DAY;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}-{:02}-{:02}Z-{pid}.jsonl",
        second_of_day / 3600,
        second_of_day / 60 % 60,
        second_of_day % 60,
    )
}

const SECONDS_PER_DAY: u64 = 86_400;

/// The proleptic Gregorian date `days` after 1970-01-01: Howard
/// Hinnant's `civil_from_days`, for days on or after the epoch. Our own
/// 15 lines rather than a date crate for one file name.
fn civil_date(days: u64) -> (u64, u64, u64) {
    // Shift the epoch to 0000-03-01, so a leap day ends its year.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = era * 400 + year_of_era + u64::from(month <= 2);
    (year, month, day)
}

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
