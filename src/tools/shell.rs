//! The `shell` tool: one allowlisted command, split into words and run
//! without a shell, confined by the sandbox. See "The shell tool" in
//! docs/ARCHITECTURE.md.

use std::ffi::OsString;
use std::fmt;
use std::io::Read;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::conversation::ToolSpec;
use crate::tools::Stop;

/// The default for `tools.shell_timeout_s`: a choice, long enough for
/// `git log` or a search over a large repository.
pub const SHELL_TIMEOUT_S_DEFAULT: u32 = 30;

/// Characters a shell would read and Jakkals doesn't: a command or an
/// allowlist entry holding one is refused.
pub const SHELL_CHARACTERS: [char; 12] =
    ['|', '&', ';', '<', '>', '`', '$', '*', '?', '[', '\n', '\r'];

/// The most kept of each stream: a fixed bound so a runaway command
/// can't fill memory, 32 times the default `limits.tool_output_bytes`.
const CAPTURE_CAP_BYTES: usize = 1024 * 1024;

/// How often a running command is checked on: short next to any
/// command worth running, long enough not to spin.
const POLL_MS: u64 = 10;

/// How long the output is waited for once the command and its group
/// are gone. The pipes close with the last process; this bounds the
/// wait if something outside the group still holds one.
const DRAIN_MS: u64 = 1000;

/// macOS's own launcher for a Seatbelt profile, at its fixed path.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The Seatbelt profile, kept beside this file so it can be read and
/// reviewed as a whole.
const SEATBELT_PROFILE: &str = include_str!("shell/seatbelt.sb");

/// How shell commands are confined.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sandbox {
    /// macOS's Seatbelt, through `sandbox-exec`.
    Seatbelt,
    /// Linux's Landlock, with a seccomp filter for sockets.
    Landlock,
    /// Word checks only.
    None,
}

impl Sandbox {
    /// This system's own sandbox, the default: none where Jakkals has
    /// none, so the profile must say `none` there.
    pub fn native() -> Option<Sandbox> {
        if cfg!(target_os = "macos") {
            Some(Sandbox::Seatbelt)
        } else if LANDLOCK_BUILT {
            Some(Sandbox::Landlock)
        } else {
            None
        }
    }
}

/// Whether this build has the Landlock sandbox: Linux, on the
/// architectures its seccomp filter knows.
const LANDLOCK_BUILT: bool = cfg!(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
));

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod landlock;

/// The shell's settings, as the profile gives them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellSettings {
    /// Each allowed command's leading words.
    pub allow: Vec<Vec<String>>,
    pub sandbox: Sandbox,
    /// Absolute paths the sandbox also lets commands read.
    pub sandbox_read: Vec<PathBuf>,
    pub timeout_s: u32,
}

/// Why a command's text can't be run as words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordsError {
    /// It holds one of [`SHELL_CHARACTERS`].
    ShellCharacter(char),
    /// Its quotes don't close.
    Unsplittable,
    Empty,
}

/// Splits `text` into words with shell quoting rules, refusing what
/// only a shell would read.
pub fn words(text: &str) -> Result<Vec<String>, WordsError> {
    if let Some(character) = text.chars().find(|c| SHELL_CHARACTERS.contains(c)) {
        return Err(WordsError::ShellCharacter(character));
    }
    let words = shlex::split(text).ok_or(WordsError::Unsplittable)?;
    if words.is_empty() {
        return Err(WordsError::Empty);
    }
    Ok(words)
}

/// Why the shell can't be set up for a run.
#[derive(Debug)]
pub enum ShellSetupError {
    /// `seatbelt` asked for on a system without it.
    SeatbeltNeedsMacos,
    /// `landlock` asked for on a system without it.
    LandlockNeedsLinux,
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    Landlock(landlock::LandlockError),
    /// A `tools.sandbox_read` path that can't be resolved.
    SandboxRead {
        path: PathBuf,
        error: std::io::Error,
    },
}

impl fmt::Display for ShellSetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SeatbeltNeedsMacos => write!(
                formatter,
                "tools.sandbox `seatbelt` needs macOS; write sandbox = \"none\" to run with word checks only"
            ),
            Self::LandlockNeedsLinux => write!(
                formatter,
                "tools.sandbox `landlock` needs Linux on x86_64 or aarch64; write sandbox = \"none\" to run with word checks only"
            ),
            #[cfg(all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            Self::Landlock(error) => match error {
                landlock::LandlockError::Abi(abi) => write!(
                    formatter,
                    "tools.sandbox `landlock` needs Landlock ABI {} (Linux 6.2) or later, enabled in the kernel; this one has {}",
                    landlock::ABI_REQUIRED,
                    if *abi == 0 {
                        "none".to_owned()
                    } else {
                        format!("ABI {abi}")
                    }
                ),
                landlock::LandlockError::Path { path, error } => {
                    write!(
                        formatter,
                        "tools.sandbox `landlock`, allowing {}: {error}",
                        path.display()
                    )
                }
                landlock::LandlockError::Syscall { call, error } => {
                    write!(formatter, "tools.sandbox `landlock`, {call}: {error}")
                }
            },
            Self::SandboxRead { path, error } => {
                write!(formatter, "tools.sandbox_read {}: {error}", path.display())
            }
        }
    }
}

impl std::error::Error for ShellSetupError {}

/// The shell, set up for one run.
pub struct Shell {
    root: PathBuf,
    settings: ShellSettings,
    /// `PATH` for commands: Jakkals's own, the only variable passed.
    path_env: Option<OsString>,
    confinement: Confinement,
}

/// A sandbox, set up.
enum Confinement {
    None,
    /// The Seatbelt profile with a rule per read path, and the
    /// `-D name=value` parameters those rules name.
    Seatbelt {
        profile: String,
        parameters: Vec<OsString>,
    },
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    Landlock(landlock::Landlock),
}

impl Shell {
    /// `root` is the resolved working directory.
    pub fn new(
        root: &Path,
        settings: &ShellSettings,
        path_env: Option<OsString>,
    ) -> Result<Self, ShellSetupError> {
        assert!(
            !settings.allow.is_empty(),
            "the profile requires an allowlist"
        );
        assert!(
            settings.timeout_s > 0,
            "the profile requires a positive timeout"
        );
        // A sandbox matches the path a file really has, so a symlinked
        // prefix is allowed where it leads.
        let mut read = Vec::with_capacity(settings.sandbox_read.len());
        for path in &settings.sandbox_read {
            read.push(
                path.canonicalize()
                    .map_err(|error| ShellSetupError::SandboxRead {
                        path: path.clone(),
                        error,
                    })?,
            );
        }
        let confinement = match settings.sandbox {
            Sandbox::None => Confinement::None,
            Sandbox::Seatbelt => {
                if !cfg!(target_os = "macos") {
                    return Err(ShellSetupError::SeatbeltNeedsMacos);
                }
                let (profile, parameters) = seatbelt(root, &read);
                Confinement::Seatbelt {
                    profile,
                    parameters,
                }
            }
            Sandbox::Landlock => landlock_confinement(root, &read)?,
        };
        Ok(Self {
            root: root.to_owned(),
            settings: settings.clone(),
            path_env,
            confinement,
        })
    }

    pub fn sandbox(&self) -> Sandbox {
        self.settings.sandbox
    }

    // Part of the prompt: names what is allowed, so the model needn't
    // guess and be refused.
    pub fn spec(&self) -> ToolSpec {
        let allowed: Vec<String> = self
            .settings
            .allow
            .iter()
            .map(|entry| format!("`{}`", entry.join(" ")))
            .collect();
        ToolSpec {
            name: "shell".to_owned(),
            description: format!(
                "Run one command in the working directory, without a shell: \
                 no pipes, redirects, variables or globs. Allowed commands: {}.",
                allowed.join(", ")
            ),
            parameters: json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    pub(crate) fn call(
        &self,
        arguments: ShellArguments,
        deadline: Instant,
    ) -> Result<String, Stop> {
        let command = &arguments.command;
        let words = words(command).map_err(|error| match error {
            WordsError::ShellCharacter(character) => Stop::Refused(format!(
                "`{command}` holds `{}`; there is no shell: run one program, \
                 with no pipes, redirects, variables or globs",
                character.escape_default()
            )),
            WordsError::Unsplittable => {
                Stop::Failed(format!("`{command}` has a quote that doesn't close"))
            }
            WordsError::Empty => Stop::Failed("the command is empty".to_owned()),
        })?;
        if !self
            .settings
            .allow
            .iter()
            .any(|entry| words.starts_with(entry))
        {
            let allowed: Vec<String> = self
                .settings
                .allow
                .iter()
                .map(|entry| entry.join(" "))
                .collect();
            return Err(Stop::Refused(format!(
                "`{command}` isn't allowed; allowed commands begin with: {}",
                allowed.join(", ")
            )));
        }

        let timeout = Duration::from_secs(u64::from(self.settings.timeout_s));
        let (stop_at, stop) = match Instant::now().checked_add(timeout) {
            Some(at) if at < deadline => (at, End::Timeout),
            _ => (deadline, End::Deadline),
        };
        if Instant::now() >= stop_at {
            return Err(Stop::Failed(end_line(stop, &self.settings)));
        }
        let mut child = self
            .command(&words)
            .spawn()
            .map_err(|error| Stop::Failed(format!("can't start `{}`: {error}", words[0])))?;
        let stdout = capture(child.stdout.take().expect("stdout is piped"));
        let stderr = capture(child.stderr.take().expect("stderr is piped"));
        let end = wait(&mut child, stop_at, stop);
        // Whatever the command left running goes with it, so a run
        // leaves nothing behind.
        kill_group(&child);
        let drain_until = Instant::now() + Duration::from_millis(DRAIN_MS);
        let stdout = drained(&stdout, drain_until);
        let stderr = drained(&stderr, drain_until);

        let mut text = stdout.text("stdout");
        if stderr.total_bytes > 0 {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("[jakkals: stderr]\n");
            text.push_str(&stderr.text("stderr"));
        }
        if end == End::Exited(0) {
            return Ok(text);
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&end_line(end, &self.settings));
        Err(Stop::Failed(text))
    }

    fn command(&self, words: &[String]) -> Command {
        let mut command = match &self.confinement {
            Confinement::None => {
                let mut command = Command::new(&words[0]);
                command.args(&words[1..]);
                command
            }
            #[cfg(all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            Confinement::Landlock(landlock) => {
                let mut command = Command::new(&words[0]);
                command.args(&words[1..]);
                landlock.confine(&mut command);
                command
            }
            Confinement::Seatbelt {
                profile,
                parameters,
            } => {
                let mut command = Command::new(SANDBOX_EXEC);
                command.arg("-p").arg(profile);
                for parameter in parameters {
                    command.arg("-D").arg(parameter);
                }
                command.arg("--").args(words);
                command
            }
        };
        command
            .current_dir(&self.root)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(path) = &self.path_env {
            command.env("PATH", path);
        }
        command
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShellArguments {
    command: String,
}

/// The profile text and its parameters: the working directory as
/// `CWD`, and each read path as `READ_n` with a rule allowing it.
/// Passed as parameters, so no path is ever quoted into the profile.
fn seatbelt(root: &Path, read: &[PathBuf]) -> (String, Vec<OsString>) {
    let mut profile = SEATBELT_PROFILE.to_owned();
    let mut parameters = vec![parameter("CWD", root)];
    for (index, path) in read.iter().enumerate() {
        let name = format!("READ_{index}");
        profile.push_str(&format!(
            "(allow file-read* (subpath (param \"{name}\")))\n"
        ));
        parameters.push(parameter(&name, path));
    }
    (profile, parameters)
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn landlock_confinement(root: &Path, read: &[PathBuf]) -> Result<Confinement, ShellSetupError> {
    landlock::Landlock::new(root, read)
        .map(Confinement::Landlock)
        .map_err(ShellSetupError::Landlock)
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn landlock_confinement(_root: &Path, _read: &[PathBuf]) -> Result<Confinement, ShellSetupError> {
    Err(ShellSetupError::LandlockNeedsLinux)
}

fn parameter(name: &str, path: &Path) -> OsString {
    let mut parameter = OsString::from(format!("{name}="));
    parameter.push(path);
    parameter
}

/// How a command ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Exited(i32),
    Signal(i32),
    Timeout,
    Deadline,
    /// Waiting on it failed; it was killed.
    Lost,
}

fn end_line(end: End, settings: &ShellSettings) -> String {
    match end {
        End::Exited(code) => format!("[jakkals: exit status {code}]"),
        End::Signal(signal) => format!("[jakkals: killed by signal {signal}]"),
        End::Timeout => format!(
            "[jakkals: killed after the {} s timeout]",
            settings.timeout_s
        ),
        End::Deadline => "[jakkals: killed at the run's deadline]".to_owned(),
        End::Lost => "[jakkals: lost track of the command; killed]".to_owned(),
    }
}

fn wait(child: &mut Child, stop_at: Instant, stop: End) -> End {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return ended(status),
            Ok(None) => {}
            Err(_) => {
                kill_group(child);
                let _ = child.wait();
                return End::Lost;
            }
        }
        if Instant::now() >= stop_at {
            kill_group(child);
            let _ = child.wait();
            return stop;
        }
        thread::sleep(Duration::from_millis(POLL_MS));
    }
}

fn ended(status: ExitStatus) -> End {
    match (status.code(), status.signal()) {
        (Some(code), _) => End::Exited(code),
        (None, Some(signal)) => End::Signal(signal),
        (None, None) => unreachable!("a Unix process exits with a status or a signal"),
    }
}

/// Kills the process group the command leads.
fn kill_group(child: &Child) {
    let group = libc::pid_t::try_from(child.id()).expect("a pid fits pid_t");
    // SAFETY: killpg takes plain integers and touches no memory of
    // ours. The group is the command's own (`process_group(0)`); an
    // error means it is already gone, which is what was wanted.
    unsafe {
        libc::killpg(group, libc::SIGKILL);
    }
}

/// One output stream as read: up to [`CAPTURE_CAP_BYTES`] of it, and
/// how long it was.
struct Captured {
    kept: Vec<u8>,
    total_bytes: u64,
    /// The stream was still open when the wait for it ended.
    open: bool,
}

impl Captured {
    fn text(&self, name: &str) -> String {
        let mut text = String::from_utf8_lossy(&self.kept).into_owned();
        let kept = u64::try_from(self.kept.len()).expect("length fits u64");
        let mut note = |line: String| {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&line);
        };
        if self.total_bytes > kept {
            note(format!(
                "[jakkals: {name} cut at {kept} of {} bytes]\n",
                self.total_bytes
            ));
        }
        if self.open {
            note(format!(
                "[jakkals: {name} still open after the command ended]\n"
            ));
        }
        text
    }
}

/// Reads `stream` on a thread of its own, so a full pipe never stalls
/// the command.
fn capture(mut stream: impl Read + Send + 'static) -> Receiver<Captured> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut total_bytes: u64 = 0;
        let mut buffer = [0; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    total_bytes += u64::try_from(read).expect("length fits u64");
                    let room = CAPTURE_CAP_BYTES - kept.len();
                    kept.extend_from_slice(&buffer[..read.min(room)]);
                }
            }
        }
        // The receiver may have given up waiting; nothing to do then.
        let _ = sender.send(Captured {
            kept,
            total_bytes,
            open: false,
        });
    });
    receiver
}

fn drained(receiver: &Receiver<Captured>, until: Instant) -> Captured {
    let wait = until.saturating_duration_since(Instant::now());
    receiver.recv_timeout(wait).unwrap_or(Captured {
        kept: Vec::new(),
        total_bytes: 0,
        open: true,
    })
}

#[cfg(test)]
mod tests;
