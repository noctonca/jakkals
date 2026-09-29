//! The shell against real programs: what it runs, what it refuses, and
//! how it ends a command. The Seatbelt tests run only on macOS.

use super::*;
use crate::conversation::ToolCall;
use crate::scripted::{TempDir, block_on};
use crate::tools::local::{LocalTool, LocalTools};
use crate::tools::{ToolOutcome, ToolStatus, Tools};

/// Time enough for any command here; the deadline has its own test.
const DEADLINE_MS: u64 = 60_000;

fn settings(allow: &[&str], sandbox: Sandbox) -> ShellSettings {
    ShellSettings {
        allow: allow
            .iter()
            .map(|entry| words(entry).expect("a valid entry"))
            .collect(),
        sandbox,
        sandbox_read: Vec::new(),
        timeout_s: 30,
    }
}

fn shell(dir: &TempDir, settings: &ShellSettings) -> LocalTools {
    LocalTools::new(
        &dir.root,
        &[LocalTool::Shell],
        Some(settings),
        std::env::var_os("PATH"),
    )
    .expect("the shell sets up")
}

fn run_by(tools: &mut LocalTools, arguments: &str, deadline_ms: u64) -> ToolOutcome {
    let call = ToolCall {
        id: "call-0".to_owned(),
        name: "shell".to_owned(),
        arguments: arguments.to_owned(),
    };
    block_on(tools.call(&call, deadline_ms))
}

fn run(tools: &mut LocalTools, command: &str) -> ToolOutcome {
    run_by(
        tools,
        &json!({ "command": command }).to_string(),
        DEADLINE_MS,
    )
}

/// Asserts the outcome's status and that its text holds `says`.
fn assert_outcome(outcome: ToolOutcome, status: ToolStatus, says: &str) {
    assert_eq!(outcome.status(), status, "{outcome:?}");
    let text = outcome.into_text();
    assert!(text.contains(says), "{text:?} says {says:?}");
}

#[test]
fn words_follow_shell_quoting_and_refuse_what_only_a_shell_reads() {
    assert_eq!(
        words(r#"git log --format="%h %s" 'a b'"#),
        Ok(vec![
            "git".to_owned(),
            "log".to_owned(),
            "--format=%h %s".to_owned(),
            "a b".to_owned()
        ])
    );
    for character in SHELL_CHARACTERS {
        assert_eq!(
            words(&format!("echo a{character}b")),
            Err(WordsError::ShellCharacter(character)),
            "{character:?}"
        );
    }
    assert_eq!(
        words("grep 'a|b'"),
        Err(WordsError::ShellCharacter('|')),
        "quoted or not"
    );
    assert_eq!(words("echo 'open"), Err(WordsError::Unsplittable));
    assert_eq!(words("  "), Err(WordsError::Empty));
}

#[test]
fn the_spec_names_the_allowed_commands() {
    let dir = TempDir::new("shell-spec");
    let tools = shell(&dir, &settings(&["git log", "rg"], Sandbox::None));
    let spec = &tools.specs()[0];
    assert_eq!(spec.name, "shell");
    assert!(
        spec.description
            .ends_with("Allowed commands: `git log`, `rg`."),
        "{:?}",
        spec.description
    );
    assert_eq!(tools.sandbox(), Some(Sandbox::None));
}

#[test]
fn a_command_runs_in_the_working_directory_with_only_path() {
    let dir = TempDir::new("shell-runs");
    dir.write("box.txt", "a cat\n");
    let mut tools = shell(
        &dir,
        &settings(&["cat", "echo", "env", "pwd"], Sandbox::None),
    );

    assert_eq!(
        run(&mut tools, "cat box.txt"),
        ToolOutcome::Ok("a cat\n".to_owned())
    );
    assert_eq!(
        run(&mut tools, r#"echo "two  spaces" 'kept'"#),
        ToolOutcome::Ok("two  spaces kept\n".to_owned())
    );
    let path = std::env::var("PATH").expect("tests run with a PATH");
    assert_eq!(
        run(&mut tools, "env"),
        ToolOutcome::Ok(format!("PATH={path}\n"))
    );
    let root = dir.root.canonicalize().expect("the root resolves");
    assert_eq!(
        run(&mut tools, "pwd"),
        ToolOutcome::Ok(format!("{}\n", root.display()))
    );
    assert_eq!(
        run(&mut tools, "cat"),
        ToolOutcome::Ok(String::new()),
        "no input: a command reading it ends at once"
    );
}

#[test]
fn commands_outside_the_allowlist_or_needing_a_shell_are_refused() {
    let dir = TempDir::new("shell-refused");
    let mut tools = shell(&dir, &settings(&["git log", "echo"], Sandbox::None));

    let cases = [
        (
            "git push",
            "isn't allowed; allowed commands begin with: git log, echo",
        ),
        ("git logfoo", "isn't allowed"),
        ("git -C / log", "isn't allowed"),
        ("gitlog", "isn't allowed"),
        ("echo a | wc", "holds `|`; there is no shell"),
        ("echo $HOME", "holds `$`"),
        ("echo *.rs", "holds `*`"),
        ("echo a > out.txt", "holds `>`"),
        ("echo a && echo b", "holds `&`"),
        ("echo a\necho b", "holds `\\n`"),
    ];
    for (command, says) in cases {
        assert_outcome(run(&mut tools, command), ToolStatus::Refused, says);
    }
    assert!(
        !dir.root.join("out.txt").exists(),
        "nothing ran for a refused command"
    );
    assert_outcome(
        run(&mut tools, "echo 'open"),
        ToolStatus::Failed,
        "doesn't close",
    );
    assert_outcome(
        run_by(&mut tools, r#"{"cmd":"echo"}"#, DEADLINE_MS),
        ToolStatus::Failed,
        "the arguments aren't valid",
    );
}

#[test]
fn a_failing_command_returns_its_output_and_status() {
    let dir = TempDir::new("shell-fails");
    let mut tools = shell(
        &dir,
        &settings(&["ls", "no-such-program-for-jakkals"], Sandbox::None),
    );

    let outcome = run(&mut tools, "ls no-such-file");
    assert_eq!(outcome.status(), ToolStatus::Failed);
    let text = outcome.into_text();
    assert!(text.starts_with("[jakkals: stderr]\n"), "{text:?}");
    assert!(text.contains("no-such-file"), "{text:?}");
    // ls's status for a missing file differs between systems.
    assert!(text.contains("\n[jakkals: exit status "), "{text:?}");
    assert!(!text.ends_with("status 0]"), "{text:?}");

    assert_outcome(
        run(&mut tools, "no-such-program-for-jakkals"),
        ToolStatus::Failed,
        "can't start `no-such-program-for-jakkals`",
    );
}

#[test]
fn a_command_past_its_timeout_or_the_deadline_is_killed() {
    let dir = TempDir::new("shell-timeout");
    let mut short = settings(&["sleep"], Sandbox::None);
    short.timeout_s = 1;
    let mut tools = shell(&dir, &short);

    let started = Instant::now();
    assert_outcome(
        run(&mut tools, "sleep 20"),
        ToolStatus::Failed,
        "[jakkals: killed after the 1 s timeout]",
    );
    assert!(started.elapsed() < Duration::from_secs(5));

    let started = Instant::now();
    assert_outcome(
        run_by(&mut tools, r#"{"command":"sleep 20"}"#, 200),
        ToolStatus::Failed,
        "[jakkals: killed at the run's deadline]",
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn what_a_command_leaves_running_is_killed_with_it() {
    let dir = TempDir::new("shell-group");
    // The script's child holds the output pipe open for 20 s after the
    // script ends: only a group kill lets the call return at once.
    dir.write("spawn.sh", "#!/bin/sh\nsleep 20 &\necho started\n");
    let script = dir.root.join("spawn.sh");
    let mut permissions = std::fs::metadata(&script).expect("script").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&script, permissions).expect("script is executable");
    let mut tools = shell(&dir, &settings(&["./spawn.sh"], Sandbox::None));

    let started = Instant::now();
    assert_eq!(
        run(&mut tools, "./spawn.sh"),
        ToolOutcome::Ok("started\n".to_owned())
    );
    assert!(
        started.elapsed() < Duration::from_millis(DRAIN_MS),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn each_stream_is_kept_up_to_its_cap() {
    let dir = TempDir::new("shell-cap");
    dir.write("big.txt", "x".repeat(CAPTURE_CAP_BYTES + 100));
    let mut tools = shell(&dir, &settings(&["cat"], Sandbox::None));

    let text = run(&mut tools, "cat big.txt").into_text();
    let (kept, note) = text.split_at(CAPTURE_CAP_BYTES);
    assert!(kept.bytes().all(|byte| byte == b'x'));
    assert_eq!(
        note,
        format!(
            "\n[jakkals: stdout cut at {CAPTURE_CAP_BYTES} of {} bytes]\n",
            CAPTURE_CAP_BYTES + 100
        )
    );
}

#[cfg(target_os = "macos")]
mod seatbelt {
    use super::*;

    #[test]
    fn reads_stay_inside_and_nothing_is_written() {
        let dir = TempDir::new("shell-seatbelt");
        dir.write("box.txt", "a cat\n");
        std::fs::write(dir.base.join("outside.txt"), "not yours\n").expect("file is writable");
        let mut tools = shell(&dir, &settings(&["cat", "touch", "ls"], Sandbox::Seatbelt));
        assert_eq!(tools.sandbox(), Some(Sandbox::Seatbelt));

        assert_eq!(
            run(&mut tools, "cat box.txt"),
            ToolOutcome::Ok("a cat\n".to_owned())
        );
        for command in [
            "cat ../outside.txt",
            "cat /etc/hosts",
            "touch new.txt",
            "ls /Users",
        ] {
            assert_outcome(
                run(&mut tools, command),
                ToolStatus::Failed,
                "Operation not permitted",
            );
        }
        assert!(!dir.root.join("new.txt").exists());
    }

    #[test]
    fn sandbox_read_opens_a_path_to_reading() {
        let dir = TempDir::new("shell-seatbelt-read");
        let extra = dir.base.join("extra");
        std::fs::create_dir(&extra).expect("dir is creatable");
        std::fs::write(extra.join("lib.txt"), "shared\n").expect("file is writable");
        let mut with_read = settings(&["cat"], Sandbox::Seatbelt);
        with_read.sandbox_read = vec![extra.clone()];
        let mut tools = shell(&dir, &with_read);

        let command = format!("cat {}", extra.join("lib.txt").display());
        assert_eq!(
            run(&mut tools, &command),
            ToolOutcome::Ok("shared\n".to_owned())
        );
    }

    #[test]
    fn a_missing_read_path_stops_the_setup() {
        let dir = TempDir::new("shell-seatbelt-missing");
        let mut with_read = settings(&["cat"], Sandbox::Seatbelt);
        with_read.sandbox_read = vec![dir.base.join("nothing")];
        let error = LocalTools::new(&dir.root, &[LocalTool::Shell], Some(&with_read), None)
            .err()
            .expect("setup fails");
        assert!(
            error.to_string().starts_with("tools.sandbox_read "),
            "{error}"
        );
    }
}

#[cfg(target_os = "linux")]
mod landlock {
    use super::*;

    #[test]
    fn reads_stay_inside_and_nothing_is_written() {
        let dir = TempDir::new("shell-landlock");
        dir.write("box.txt", "a cat\n");
        std::fs::write(dir.base.join("outside.txt"), "not yours\n").expect("file is writable");
        let mut tools = shell(
            &dir,
            &settings(
                &["cat", "touch", "ls", "truncate", "mkdir"],
                Sandbox::Landlock,
            ),
        );
        assert_eq!(tools.sandbox(), Some(Sandbox::Landlock));

        assert_eq!(
            run(&mut tools, "cat box.txt"),
            ToolOutcome::Ok("a cat\n".to_owned())
        );
        for command in [
            "cat ../outside.txt",
            "cat /etc/passwd",
            "ls /home",
            "touch new.txt",
            "mkdir new",
            "truncate -s 0 box.txt",
        ] {
            assert_outcome(
                run(&mut tools, command),
                ToolStatus::Failed,
                "Permission denied",
            );
        }
        assert!(!dir.root.join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.root.join("box.txt")).expect("still there"),
            "a cat\n"
        );
    }

    #[test]
    fn no_socket_opens() {
        let dir = TempDir::new("shell-landlock-socket");
        // Perl is on every Linux that has git. Address families 2 and 1
        // are the internet's and Unix's; type 1 is a stream.
        dir.write(
            "socket.pl",
            "for my $family (2, 1) { socket(my $s, $family, 1, 0) or print \"$family: $!\\n\" }\n",
        );
        let mut tools = shell(&dir, &settings(&["perl"], Sandbox::Landlock));

        assert_eq!(
            run(&mut tools, "perl socket.pl"),
            ToolOutcome::Ok("2: Operation not permitted\n1: Operation not permitted\n".to_owned())
        );
        let mut open = shell(&dir, &settings(&["perl"], Sandbox::None));
        assert_eq!(
            run(&mut open, "perl socket.pl"),
            ToolOutcome::Ok(String::new()),
            "the same script opens both without the sandbox"
        );
    }

    #[test]
    fn sandbox_read_opens_a_path_to_reading() {
        let dir = TempDir::new("shell-landlock-read");
        let extra = dir.base.join("extra");
        std::fs::create_dir(&extra).expect("dir is creatable");
        std::fs::write(extra.join("lib.txt"), "shared\n").expect("file is writable");
        let mut with_read = settings(&["cat"], Sandbox::Landlock);
        with_read.sandbox_read = vec![extra.clone()];
        let mut tools = shell(&dir, &with_read);

        let command = format!("cat {}", extra.join("lib.txt").display());
        assert_eq!(
            run(&mut tools, &command),
            ToolOutcome::Ok("shared\n".to_owned())
        );
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn landlock_needs_linux() {
    let dir = TempDir::new("shell-no-landlock");
    let error = LocalTools::new(
        &dir.root,
        &[LocalTool::Shell],
        Some(&settings(&["cat"], Sandbox::Landlock)),
        None,
    )
    .err()
    .expect("setup fails");
    assert!(error.to_string().contains("needs Linux"), "{error}");
}

#[cfg(not(target_os = "macos"))]
#[test]
fn seatbelt_needs_macos() {
    let dir = TempDir::new("shell-no-seatbelt");
    let error = LocalTools::new(
        &dir.root,
        &[LocalTool::Shell],
        Some(&settings(&["cat"], Sandbox::Seatbelt)),
        None,
    )
    .err()
    .expect("setup fails");
    assert!(error.to_string().contains("needs macOS"), "{error}");
}
