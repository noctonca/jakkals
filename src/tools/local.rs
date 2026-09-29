//! The local tools: `read`, `list`, `search` and `shell`, each confined
//! to the run's working directory. See "The local tools" in
//! docs/ARCHITECTURE.md for what each does, refuses and is bounded by;
//! the shell is in [`super::shell`].

use std::ffi::OsString;
use std::fmt::{self, Write as _};
use std::fs::File;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use grep_regex::RegexMatcher;
use grep_searcher::sinks::Lossy;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::conversation::{ToolCall, ToolSpec};
use crate::tools::mcp::McpServerRecord;
use crate::tools::shell::{Sandbox, Shell, ShellSettings, ShellSetupError};
use crate::tools::{Failure, Refusal, Stop, ToolOutcome, Tools};

/// `read`'s largest file: 1 MiB. A choice: 32 times the default
/// `limits.tool_output_bytes`, room for any hand-written source file.
/// A larger one is generated or data, better searched than read.
const READ_CAP_BYTES: u64 = 1024 * 1024;

/// `list`'s most entries: a choice, enough for a mid-sized project's
/// tree; a larger one is better listed a directory at a time.
const LIST_CAP_ENTRIES: u32 = 1000;

/// `search`'s most hits: a choice, about 200 lines of context; past
/// that the pattern wants narrowing.
const SEARCH_CAP_HITS: u32 = 200;

/// The most of one hit's line shown: a choice, wide enough for any
/// hand-written line; minified code would otherwise fill the result.
const SEARCH_LINE_SHOWN_BYTES: usize = 500;

/// The searcher's buffer bound: a file with a line longer than this is
/// skipped rather than held in memory. A choice, far past any
/// hand-written line.
const SEARCH_LINE_CAP_BYTES: usize = 1024 * 1024;

/// A local tool a profile can offer, named as the profile writes it.
/// Ordered as the tools are offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalTool {
    Read,
    List,
    Search,
    Shell,
}

impl LocalTool {
    pub fn name(self) -> &'static str {
        match self {
            LocalTool::Read => "read",
            LocalTool::List => "list",
            LocalTool::Search => "search",
            LocalTool::Shell => "shell",
        }
    }

    // The descriptions are part of the prompt: short, and saying only
    // what the model needs to call the tool well. The shell's names its
    // allowlist, so it is the shell's own.
    fn spec(self, shell: Option<&Shell>) -> ToolSpec {
        let path = json!({
            "type": "string",
            "description": "Relative to the working directory."
        });
        let (description, parameters) = match self {
            LocalTool::Read => (
                "Read a text file. start_line (from 1) and line_count read part of it.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": path,
                        "start_line": {"type": "integer", "minimum": 1},
                        "line_count": {"type": "integer", "minimum": 1}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
            LocalTool::List => (
                "List the files under a directory (default: all), recursively, \
                 skipping hidden and ignored ones. Directories end in /. \
                 depth 1 lists only the directory's own entries.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": path,
                        "depth": {"type": "integer", "minimum": 1}
                    },
                    "additionalProperties": false
                }),
            ),
            LocalTool::Search => (
                "Search the files under a path (default: all) for a regular expression \
                 (Rust regex syntax), skipping hidden and ignored ones. \
                 Returns path:line:text per matching line.",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "path": path
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }),
            ),
            LocalTool::Shell => return shell.expect("the shell is set up").spec(),
        };
        ToolSpec {
            name: self.name().to_owned(),
            description: description.to_owned(),
            parameters,
        }
    }
}

/// The local tools a run offers, working in one directory.
pub struct LocalTools {
    /// `--cwd`, resolved once: every path a tool takes is inside it.
    root: PathBuf,
    offered: Vec<LocalTool>,
    specs: Vec<ToolSpec>,
    /// Set up when `shell` is offered.
    shell: Option<Shell>,
}

/// Why the local tools can't be set up for a run.
#[derive(Debug)]
pub enum ToolsSetupError {
    /// `--cwd` can't be resolved.
    WorkingDirectory(std::io::Error),
    Shell(ShellSetupError),
}

impl fmt::Display for ToolsSetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkingDirectory(error) => write!(formatter, "--cwd: {error}"),
            Self::Shell(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ToolsSetupError {}

impl LocalTools {
    /// `offered` is in order, each tool once, as the profile gives it,
    /// with the shell's settings exactly when `shell` is offered.
    /// `path_env` is the `PATH` shell commands get.
    pub fn new(
        cwd: &Path,
        offered: &[LocalTool],
        shell: Option<&ShellSettings>,
        path_env: Option<OsString>,
    ) -> Result<Self, ToolsSetupError> {
        assert!(
            offered.windows(2).all(|pair| pair[0] < pair[1]),
            "the profile offers each tool once, in order"
        );
        assert_eq!(
            offered.contains(&LocalTool::Shell),
            shell.is_some(),
            "the profile gives the shell's settings exactly when it offers the shell"
        );
        let root = cwd
            .canonicalize()
            .map_err(ToolsSetupError::WorkingDirectory)?;
        let shell = shell
            .map(|settings| Shell::new(&root, settings, path_env))
            .transpose()
            .map_err(ToolsSetupError::Shell)?;
        Ok(Self {
            specs: offered
                .iter()
                .map(|tool| tool.spec(shell.as_ref()))
                .collect(),
            root,
            offered: offered.to_vec(),
            shell,
        })
    }

    fn read(&self, arguments: ReadArguments) -> Result<String, Stop> {
        let shown = &arguments.path;
        let path = self.resolve(shown)?;
        if path.is_dir() {
            return Err(Stop::Failed(
                Failure::WrongType,
                format!("`{shown}` is a directory; list it instead"),
            ));
        }
        let too_large = |bytes: u64| {
            Stop::Failed(
                Failure::TooLarge,
                format!(
                    "`{shown}` is {bytes} bytes, past read's cap of {READ_CAP_BYTES}; search it instead"
                ),
            )
        };
        let file = File::open(&path).map_err(|error| io_failed(shown, &error))?;
        let length = file
            .metadata()
            .map_err(|error| io_failed(shown, &error))?
            .len();
        if length > READ_CAP_BYTES {
            return Err(too_large(length));
        }
        // Taken one past the cap, in case the file grew since.
        let mut bytes = Vec::new();
        file.take(READ_CAP_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| io_failed(shown, &error))?;
        let length = u64::try_from(bytes.len()).expect("length fits u64");
        if length > READ_CAP_BYTES {
            return Err(too_large(length));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| Stop::Failed(Failure::NotText, format!("`{shown}` isn't UTF-8 text")))?;
        if arguments.start_line.is_none() && arguments.line_count.is_none() {
            return Ok(text);
        }

        let start_line = arguments.start_line.unwrap_or(1);
        if start_line == 0 {
            return Err(Stop::Failed(
                Failure::Arguments,
                "start_line counts from 1".to_owned(),
            ));
        }
        if arguments.line_count == Some(0) {
            return Err(Stop::Failed(
                Failure::Arguments,
                "line_count must be at least 1".to_owned(),
            ));
        }
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let first = usize::try_from(start_line - 1).expect("u32 fits usize");
        if first >= lines.len() {
            return Err(Stop::Failed(
                Failure::Arguments,
                format!(
                    "`{shown}` has {} lines; start_line {start_line} is past its end",
                    lines.len()
                ),
            ));
        }
        let end = match arguments.line_count {
            None => lines.len(),
            Some(count) => first
                .saturating_add(usize::try_from(count).expect("u32 fits usize"))
                .min(lines.len()),
        };
        Ok(lines[first..end].concat())
    }

    fn list(&self, arguments: ListArguments, deadline: Instant) -> Result<String, Stop> {
        let shown = arguments.path.as_deref().unwrap_or(".");
        let path = self.resolve(shown)?;
        if !path.is_dir() {
            return Err(Stop::Failed(
                Failure::WrongType,
                format!("`{shown}` is a file; read it instead"),
            ));
        }
        if arguments.depth == Some(0) {
            return Err(Stop::Failed(
                Failure::Arguments,
                "depth must be at least 1".to_owned(),
            ));
        }

        let mut text = String::new();
        let mut entries: u32 = 0;
        let mut skipped: u32 = 0;
        let mut walk = walk(&path);
        walk.max_depth(
            arguments
                .depth
                .map(|depth| usize::try_from(depth).expect("u32 fits usize")),
        );
        for entry in walk.build() {
            if Instant::now() >= deadline {
                return Err(deadline_passed("list"));
            }
            let Ok(entry) = entry else {
                skipped += 1;
                continue;
            };
            if entry.depth() == 0 {
                continue;
            }
            if entries == LIST_CAP_ENTRIES {
                writeln!(
                    text,
                    "[jakkals: list stopped at {LIST_CAP_ENTRIES} entries]"
                )
                .expect("writing to a String");
                break;
            }
            entries += 1;
            let marker = match entry.file_type() {
                Some(kind) if kind.is_dir() => "/",
                Some(kind) if kind.is_symlink() => "@",
                _ => "",
            };
            writeln!(text, "{}{marker}", self.shown(entry.path())).expect("writing to a String");
        }
        if entries == 0 {
            text.push_str("(no entries)\n");
        }
        note_skipped(&mut text, skipped);
        Ok(text)
    }

    fn search(&self, arguments: SearchArguments, deadline: Instant) -> Result<String, Stop> {
        let shown = arguments.path.as_deref().unwrap_or(".");
        let path = self.resolve(shown)?;
        let matcher = RegexMatcher::new_line_matcher(&arguments.pattern).map_err(|error| {
            Stop::Failed(
                Failure::Arguments,
                format!("the pattern isn't valid: {error}"),
            )
        })?;
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(true)
            .heap_limit(Some(SEARCH_LINE_CAP_BYTES))
            .build();

        let mut text = String::new();
        let mut hits: u32 = 0;
        let mut skipped: u32 = 0;
        let mut stopped = false;
        for entry in walk(&path).build() {
            if Instant::now() >= deadline {
                return Err(deadline_passed("search"));
            }
            let Ok(entry) = entry else {
                skipped += 1;
                continue;
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let file = self.shown(entry.path());
            let searched = searcher.search_path(
                &matcher,
                entry.path(),
                Lossy(|line_number, line| {
                    if hits == SEARCH_CAP_HITS {
                        stopped = true;
                        return Ok(false);
                    }
                    hits += 1;
                    writeln!(text, "{file}:{line_number}:{}", shown_line(line))
                        .expect("writing to a String");
                    Ok(true)
                }),
            );
            if searched.is_err() {
                skipped += 1;
            }
            if stopped {
                writeln!(text, "[jakkals: search stopped at {SEARCH_CAP_HITS} hits]")
                    .expect("writing to a String");
                break;
            }
        }
        if hits == 0 {
            text.push_str("No matches.\n");
        }
        note_skipped(&mut text, skipped);
        Ok(text)
    }

    /// The path a tool was given, resolved inside the root, or why not.
    fn resolve(&self, shown: &str) -> Result<PathBuf, Stop> {
        let relative = Path::new(shown);
        for component in relative.components() {
            match component {
                Component::Normal(_) | Component::CurDir => {}
                Component::ParentDir => {
                    return Err(Stop::Refused(
                        Refusal::OutsideCwd,
                        format!("`{shown}` holds `..`; paths stay inside the working directory"),
                    ));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(Stop::Refused(
                        Refusal::OutsideCwd,
                        format!(
                            "`{shown}` is absolute; give a path relative to the working directory"
                        ),
                    ));
                }
            }
        }
        // Resolving follows every symlink, so the check below sees where
        // the path really leads.
        let resolved = self
            .root
            .join(relative)
            .canonicalize()
            .map_err(|error| io_failed(shown, &error))?;
        if !resolved.starts_with(&self.root) {
            return Err(Stop::Refused(
                Refusal::OutsideCwd,
                format!("`{shown}` leads outside the working directory"),
            ));
        }
        Ok(resolved)
    }

    /// A path inside the root as the model sees it: relative to the root.
    fn shown(&self, path: &Path) -> String {
        let relative = path
            .strip_prefix(&self.root)
            .expect("walks start inside the root and don't follow symlinks");
        if relative.as_os_str().is_empty() {
            ".".to_owned()
        } else {
            relative.to_string_lossy().into_owned()
        }
    }
}

impl Tools for LocalTools {
    fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    fn sandbox(&self) -> Option<Sandbox> {
        self.shell.as_ref().map(Shell::sandbox)
    }

    fn servers(&self) -> Vec<McpServerRecord> {
        Vec::new()
    }

    async fn call(&mut self, call: &ToolCall, deadline_ms: u64) -> ToolOutcome {
        let deadline = Instant::now() + Duration::from_millis(deadline_ms);
        let tool = self
            .offered
            .iter()
            .copied()
            .find(|tool| tool.name() == call.name)
            .expect("the loop calls only the tools offered");
        let result = match tool {
            LocalTool::Read => arguments(&call.arguments).and_then(|parsed| self.read(parsed)),
            LocalTool::List => {
                arguments(&call.arguments).and_then(|parsed| self.list(parsed, deadline))
            }
            LocalTool::Search => {
                arguments(&call.arguments).and_then(|parsed| self.search(parsed, deadline))
            }
            LocalTool::Shell => arguments(&call.arguments).and_then(|parsed| {
                self.shell
                    .as_ref()
                    .expect("the shell is set up")
                    .call(parsed, deadline)
            }),
        };
        result.into()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArguments {
    path: String,
    start_line: Option<u32>,
    line_count: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArguments {
    path: Option<String>,
    depth: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    pattern: String,
    path: Option<String>,
}

fn arguments<T: DeserializeOwned>(text: &str) -> Result<T, Stop> {
    serde_json::from_str(text).map_err(|error| {
        Stop::Failed(
            Failure::Arguments,
            format!("the arguments aren't valid: {error}"),
        )
    })
}

/// A walk from `path` down, skipping hidden and ignored entries and
/// never following a symlink. Only the ignore files inside `path` are
/// read: not git's global ones nor a parent directory's, so what a
/// tool sees doesn't depend on the machine or on where `--cwd` sits.
fn walk(path: &Path) -> WalkBuilder {
    let mut walk = WalkBuilder::new(path);
    walk.parents(false)
        .git_global(false)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_name(|left, right| left.cmp(right));
    walk
}

fn io_failed(shown: &str, error: &std::io::Error) -> Stop {
    match error.kind() {
        std::io::ErrorKind::NotFound => {
            Stop::Failed(Failure::NotFound, format!("`{shown}` doesn't exist"))
        }
        _ => Stop::Failed(Failure::Io, format!("`{shown}`: {error}")),
    }
}

fn deadline_passed(tool: &str) -> Stop {
    Stop::Failed(
        Failure::Deadline,
        format!("jakkals: the run's deadline passed; `{tool}` stopped"),
    )
}

fn note_skipped(text: &mut String, skipped: u32) {
    if skipped > 0 {
        writeln!(text, "[jakkals: {skipped} entries couldn't be read]")
            .expect("writing to a String");
    }
}

/// One hit's line without its line ending, cut at a character boundary.
fn shown_line(line: &str) -> String {
    let line = line.trim_end_matches(['\n', '\r']);
    if line.len() <= SEARCH_LINE_SHOWN_BYTES {
        return line.to_owned();
    }
    let mut end = SEARCH_LINE_SHOWN_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

#[cfg(test)]
mod tests;
