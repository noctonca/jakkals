//! The local tools against a real directory: what each returns, what
//! it refuses, and where it stops.

use std::os::unix::fs::symlink;

use super::*;
use crate::scripted::{TempDir, block_on};
use crate::tools::ToolStatus;

const ALL: [LocalTool; 3] = [LocalTool::Read, LocalTool::List, LocalTool::Search];

/// Time enough for any call here; the deadline has its own tests.
const DEADLINE_MS: u64 = 60_000;

fn tools(dir: &TempDir) -> LocalTools {
    LocalTools::new(&dir.root, &ALL, None, None).expect("the root resolves")
}

fn call_by(tools: &mut LocalTools, name: &str, arguments: &str, deadline_ms: u64) -> ToolOutcome {
    let call = ToolCall {
        id: "call-0".to_owned(),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    };
    block_on(tools.call(&call, deadline_ms))
}

fn call(tools: &mut LocalTools, name: &str, arguments: serde_json::Value) -> ToolOutcome {
    call_by(tools, name, &arguments.to_string(), DEADLINE_MS)
}

fn ok(outcome: ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Ok(text) => text,
        other => panic!("the call succeeds, not {other:?}"),
    }
}

/// Asserts the outcome's status and that its text holds `says`.
fn assert_outcome(outcome: ToolOutcome, status: ToolStatus, says: &str) {
    assert_eq!(outcome.status(), status, "{outcome:?}");
    let text = outcome.into_text();
    assert!(text.contains(says), "{text:?} says {says:?}");
}

#[test]
fn specs_are_the_tools_offered_in_order() {
    let dir = TempDir::new("specs");
    let names = |tools: &LocalTools| -> Vec<String> {
        tools.specs().iter().map(|spec| spec.name.clone()).collect()
    };
    assert_eq!(names(&tools(&dir)), ["read", "list", "search"]);
    let some = LocalTools::new(&dir.root, &[LocalTool::Read, LocalTool::Search], None, None)
        .expect("the root resolves");
    assert_eq!(names(&some), ["read", "search"]);
    let none = LocalTools::new(&dir.root, &[], None, None).expect("the root resolves");
    assert!(none.specs().is_empty());
}

#[test]
fn a_missing_working_directory_is_an_error() {
    let dir = TempDir::new("missing-root");
    assert!(matches!(
        LocalTools::new(&dir.root.join("nothing"), &ALL, None, None),
        Err(ToolsSetupError::WorkingDirectory(_))
    ));
}

#[test]
fn read_returns_the_file_or_its_lines() {
    let dir = TempDir::new("read");
    dir.write("src/box.txt", "one\ntwo\nthree\nfour\n");
    let mut tools = tools(&dir);

    assert_eq!(
        ok(call(&mut tools, "read", json!({"path": "src/box.txt"}))),
        "one\ntwo\nthree\nfour\n"
    );
    assert_eq!(
        ok(call(
            &mut tools,
            "read",
            json!({"path": "./src/box.txt", "start_line": 2, "line_count": 2})
        )),
        "two\nthree\n"
    );
    assert_eq!(
        ok(call(
            &mut tools,
            "read",
            json!({"path": "src/box.txt", "start_line": 3})
        )),
        "three\nfour\n"
    );
    assert_eq!(
        ok(call(
            &mut tools,
            "read",
            json!({"path": "src/box.txt", "line_count": 1})
        )),
        "one\n"
    );
    assert_eq!(
        ok(call(
            &mut tools,
            "read",
            json!({"path": "src/box.txt", "start_line": 4, "line_count": 9})
        )),
        "four\n",
        "a count past the end reads to the end"
    );
}

#[test]
fn read_fails_on_what_it_cant_read() {
    let dir = TempDir::new("read-fails");
    dir.write("box.txt", "one\ntwo\n");
    dir.write("latin1.txt", b"caf\xe9\n");
    dir.write(
        "huge.txt",
        vec![b'a'; usize::try_from(READ_CAP_BYTES).expect("fits usize") + 1],
    );
    std::fs::create_dir(dir.root.join("empty")).expect("dir is creatable");
    let mut tools = tools(&dir);

    let cases = [
        (json!({"path": "nothing.txt"}), "doesn't exist"),
        (json!({"path": "empty"}), "is a directory"),
        (json!({"path": "latin1.txt"}), "isn't UTF-8"),
        (
            json!({"path": "huge.txt"}),
            "1048577 bytes, past read's cap",
        ),
        (json!({"path": "box.txt", "start_line": 3}), "has 2 lines"),
        (json!({"path": "box.txt", "start_line": 0}), "counts from 1"),
        (json!({"path": "box.txt", "line_count": 0}), "at least 1"),
        (
            json!({"path": "box.txt", "lines": 3}),
            "unknown field `lines`",
        ),
        (json!({}), "missing field `path`"),
    ];
    for (arguments, says) in cases {
        assert_outcome(
            call(&mut tools, "read", arguments),
            ToolStatus::Failed,
            says,
        );
    }
    assert_outcome(
        call_by(&mut tools, "read", "box.txt", DEADLINE_MS),
        ToolStatus::Failed,
        "the arguments aren't valid",
    );
}

#[test]
fn paths_outside_the_working_directory_are_refused() {
    let dir = TempDir::new("outside");
    std::fs::write(dir.base.join("secret.txt"), "not yours\n").expect("file is writable");
    dir.write("inside.txt", "yours\n");
    symlink(dir.base.join("secret.txt"), dir.root.join("leak.txt")).expect("symlink");
    symlink(&dir.base, dir.root.join("up")).expect("symlink");
    symlink(dir.root.join("inside.txt"), dir.root.join("alias.txt")).expect("symlink");
    let mut tools = tools(&dir);

    let absolute = dir.root.join("inside.txt").display().to_string();
    let cases = [
        ("../secret.txt", "holds `..`"),
        ("sub/../inside.txt", "holds `..`"),
        (absolute.as_str(), "is absolute"),
        ("leak.txt", "leads outside"),
        ("up/secret.txt", "leads outside"),
    ];
    for (path, says) in cases {
        for (tool, arguments) in [
            ("read", json!({"path": path})),
            ("list", json!({"path": path})),
            ("search", json!({"pattern": "yours", "path": path})),
        ] {
            assert_outcome(call(&mut tools, tool, arguments), ToolStatus::Refused, says);
        }
    }
    assert_eq!(
        ok(call(&mut tools, "read", json!({"path": "alias.txt"}))),
        "yours\n",
        "a symlink that stays inside is followed"
    );
}

#[test]
fn list_walks_sorted_skipping_hidden_and_ignored() {
    let dir = TempDir::new("list");
    dir.write("b.txt", "");
    dir.write("a/one.rs", "");
    dir.write("a/deep/two.rs", "");
    dir.write(".hidden/secret", "");
    dir.write(".gitignore", "target/\n*.log\n");
    dir.write("target/debug/out", "");
    dir.write("run.log", "");
    symlink(dir.base.clone(), dir.root.join("link")).expect("symlink");
    let mut tools = tools(&dir);

    assert_eq!(
        ok(call(&mut tools, "list", json!({}))),
        "a/\na/deep/\na/deep/two.rs\na/one.rs\nb.txt\nlink@\n"
    );
    assert_eq!(
        ok(call(&mut tools, "list", json!({"depth": 1}))),
        "a/\nb.txt\nlink@\n"
    );
    assert_eq!(
        ok(call(&mut tools, "list", json!({"path": "a", "depth": 1}))),
        "a/deep/\na/one.rs\n",
        "paths stay relative to the working directory"
    );
    assert_eq!(
        ok(call(&mut tools, "list", json!({"path": ".hidden"}))),
        ".hidden/secret\n",
        "a directory named outright is listed, hidden or not"
    );
    std::fs::create_dir(dir.root.join("empty")).expect("dir is creatable");
    assert_eq!(
        ok(call(&mut tools, "list", json!({"path": "empty"}))),
        "(no entries)\n"
    );
    assert_outcome(
        call(&mut tools, "list", json!({"path": "b.txt"})),
        ToolStatus::Failed,
        "is a file",
    );
    assert_outcome(
        call(&mut tools, "list", json!({"depth": 0})),
        ToolStatus::Failed,
        "at least 1",
    );
}

#[test]
fn list_stops_at_its_cap() {
    let dir = TempDir::new("list-cap");
    for index in 0..=LIST_CAP_ENTRIES {
        dir.write(&format!("f{index:04}"), "");
    }
    let mut tools = tools(&dir);

    let text = ok(call(&mut tools, "list", json!({})));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1001);
    assert_eq!(lines[999], "f0999");
    assert_eq!(lines[1000], "[jakkals: list stopped at 1000 entries]");
}

#[test]
fn search_returns_each_matching_line() {
    let dir = TempDir::new("search");
    dir.write(
        "src/lib.rs",
        "fn cat() {}\nfn dog() {}\nfn catalogue() {}\n",
    );
    dir.write("notes.md", "A cat sat.\r\n");
    dir.write("binary.dat", b"cat\0cat\n");
    dir.write(".gitignore", "ignored.txt\n");
    dir.write("ignored.txt", "cat\n");
    dir.write(".hidden", "cat\n");
    let mut tools = tools(&dir);

    assert_eq!(
        ok(call(&mut tools, "search", json!({"pattern": "cat"}))),
        "notes.md:1:A cat sat.\nsrc/lib.rs:1:fn cat() {}\nsrc/lib.rs:3:fn catalogue() {}\n"
    );
    assert_eq!(
        ok(call(
            &mut tools,
            "search",
            json!({"pattern": r"fn \w+\(", "path": "src/lib.rs"})
        )),
        "src/lib.rs:1:fn cat() {}\nsrc/lib.rs:2:fn dog() {}\nsrc/lib.rs:3:fn catalogue() {}\n",
        "a file can be searched on its own"
    );
    assert_eq!(
        ok(call(&mut tools, "search", json!({"pattern": "wolf"}))),
        "No matches.\n"
    );
    assert_outcome(
        call(&mut tools, "search", json!({"pattern": "("})),
        ToolStatus::Failed,
        "the pattern isn't valid",
    );
    assert_outcome(
        call(&mut tools, "search", json!({"path": "src"})),
        ToolStatus::Failed,
        "missing field `pattern`",
    );
}

#[test]
fn search_stops_at_its_caps() {
    let dir = TempDir::new("search-cap");
    dir.write("many.txt", "hit\n".repeat(250));
    let long = format!("hit {}\n", "é".repeat(400));
    dir.write("wide.txt", &long);
    dir.write(
        "huge-line.txt",
        format!("hit {}\n", "x".repeat(SEARCH_LINE_CAP_BYTES * 2)),
    );
    let mut tools = tools(&dir);

    let text = ok(call(
        &mut tools,
        "search",
        json!({"pattern": "hit", "path": "many.txt"}),
    ));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 201);
    assert_eq!(lines[199], "many.txt:200:hit");
    assert_eq!(lines[200], "[jakkals: search stopped at 200 hits]");

    let text = ok(call(
        &mut tools,
        "search",
        json!({"pattern": "hit", "path": "wide.txt"}),
    ));
    let shown = text
        .trim_end()
        .strip_prefix("wide.txt:1:")
        .expect("one hit");
    assert!(shown.ends_with('…'), "{shown:?}");
    assert!(shown.len() <= SEARCH_LINE_SHOWN_BYTES + '…'.len_utf8());

    assert_eq!(
        ok(call(
            &mut tools,
            "search",
            json!({"pattern": "hit", "path": "huge-line.txt"})
        )),
        "No matches.\n[jakkals: 1 entries couldn't be read]\n",
        "a line past the buffer's cap skips its file"
    );
}

#[test]
fn a_passed_deadline_stops_a_walk() {
    let dir = TempDir::new("deadline");
    dir.write("a.txt", "cat\n");
    let mut tools = tools(&dir);

    assert_outcome(
        call_by(&mut tools, "list", "{}", 0),
        ToolStatus::Failed,
        "deadline passed; `list` stopped",
    );
    assert_outcome(
        call_by(&mut tools, "search", r#"{"pattern":"cat"}"#, 0),
        ToolStatus::Failed,
        "deadline passed; `search` stopped",
    );
}

#[test]
fn the_loop_hands_a_read_back_to_the_model() {
    use crate::conversation::Message;
    use crate::events::Outcome;
    use crate::run::{Limits, Task, run};
    use crate::scripted::{KeptSink, ScriptedProvider, VirtualClock, answer, reply};
    use crate::transcript::NoTranscript;

    let dir = TempDir::new("loop");
    dir.write("box.txt", "a cat\n");
    let mut tools = tools(&dir);
    let clock = VirtualClock::default();
    let mut read = reply(100, 10, None);
    read.tool_calls = vec![ToolCall {
        id: "call-0".to_owned(),
        name: "read".to_owned(),
        arguments: r#"{"path":"box.txt"}"#.to_owned(),
    }];
    let mut provider = ScriptedProvider::new(&clock)
        .then(Ok(read), 10)
        .then(Ok(answer("A cat.", reply(120, 5, None))), 10);
    let task = Task {
        system_prompt: "",
        prompt: "What is in the box?",
        model: "test/model",
        profile_hash: "hash",
    };
    let limits = Limits {
        steps: 5,
        wall_s: 60,
        cost_nano_usd: None,
        tokens: None,
        context_tokens: None,
        tool_output_bytes: 1000,
    };

    let outcome = block_on(run(
        &task,
        &limits,
        &mut provider,
        &mut tools,
        &clock,
        &mut KeptSink::default(),
        &mut NoTranscript,
    ));

    assert_eq!(outcome, Outcome::Done);
    assert_eq!(
        provider.seen[1].messages.last(),
        Some(&Message::Tool {
            call_id: "call-0".to_owned(),
            text: "a cat\n".to_owned(),
        })
    );
}
