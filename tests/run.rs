//! `jakkals run` end to end: the built binary, a profile on disk and a
//! fake provider on 127.0.0.1. Checks the exit status, the event lines
//! on stdout and what goes to stderr. No network beyond the loopback.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;

use serde_json::{Value, json};

/// Answers each connection with the next canned (status, body).
fn serve(replies: Vec<(u16, Value)>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback port");
    let address = listener.local_addr().expect("bound address");
    thread::spawn(move || {
        for (status, body) in replies {
            let (stream, _) = listener.accept().expect("a connection");
            let mut reader = BufReader::new(stream.try_clone().expect("stream clones"));
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header line");
                let line = line.trim_end().to_ascii_lowercase();
                if line.is_empty() {
                    break;
                }
                if let Some(value) = line.strip_prefix("content-length:") {
                    length = value.trim().parse().expect("length is a number");
                }
            }
            let mut request = vec![0; length];
            reader.read_exact(&mut request).expect("request body");
            let body = body.to_string();
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 {status} Canned\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("response is written");
        }
    });
    address
}

fn usage(cost: f64) -> Value {
    json!({"prompt_tokens": 10, "completion_tokens": 5, "cost": cost})
}

fn answer(text: &str) -> Value {
    json!({
        "id": "gen-answer",
        "model": "test/model",
        "choices": [{"message": {"content": text}, "finish_reason": "stop"}],
        "usage": usage(0.0001)
    })
}

fn tool_call(name: &str) -> Value {
    tool_call_with(name, "{}")
}

fn tool_call_with(name: &str, arguments: &str) -> Value {
    json!({
        "id": "gen-call",
        "model": "test/model",
        "choices": [{
            "message": {"content": null, "tool_calls": [
                {"id": "call-0", "type": "function", "function": {"name": name, "arguments": arguments}}
            ]},
            "finish_reason": "tool_calls"
        }],
        "usage": usage(0.0001)
    })
}

/// A directory of its own under the system's temporary directory.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("jakkals-run-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&path).expect("temp dir is creatable");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(dir: &TempDir, profile: &str, env: &[(&str, &str)]) -> Ran {
    run_in(dir, &dir.0, profile, env)
}

struct Ran {
    status: i32,
    events: Vec<Value>,
    stderr: String,
}

fn run_in(dir: &TempDir, cwd: &std::path::Path, profile: &str, env: &[(&str, &str)]) -> Ran {
    run_with(dir, cwd, profile, env, &[])
}

/// A run with `extra` arguments after the usual ones.
fn run_with(
    dir: &TempDir,
    cwd: &std::path::Path,
    profile: &str,
    env: &[(&str, &str)],
    extra: &[OsString],
) -> Ran {
    let profile_path = dir.0.join("profile.toml");
    std::fs::write(&profile_path, profile).expect("profile is writable");
    let Output {
        status,
        stdout,
        stderr,
    } = Command::new(env!("CARGO_BIN_EXE_jakkals"))
        .args([
            "run",
            "--model",
            "test/model",
            "--prompt",
            "What is in the box?",
        ])
        .arg("--profile")
        .arg(&profile_path)
        .arg("--cwd")
        .arg(cwd)
        .args(extra)
        .env_remove("JAKKALS_TEST_KEY")
        .envs(env.iter().copied())
        .output()
        .expect("jakkals runs");
    let events = String::from_utf8(stdout)
        .expect("stdout is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect();
    Ran {
        status: status.code().expect("jakkals exits, not killed"),
        events,
        stderr: String::from_utf8(stderr).expect("stderr is UTF-8"),
    }
}

fn profile(address: SocketAddr, steps: u32) -> String {
    format!(
        "system_prompt = \"You are a test.\"\n\n[limits]\nsteps = {steps}\nwall_s = 30\n\n\
         [provider]\nbase_url = \"http://{address}/v1\"\napi_key_env = \"JAKKALS_TEST_KEY\"\n"
    )
}

fn types(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["type"].as_str().expect("every event has a type"))
        .collect()
}

const KEY: [(&str, &str); 1] = [("JAKKALS_TEST_KEY", "test-key")];

#[test]
fn an_answer_ends_the_run_done_with_status_0() {
    let dir = TempDir::new("answer");
    let address = serve(vec![(200, answer("It is empty."))]);
    let ran = run(&dir, &profile(address, 5), &KEY);

    assert_eq!(ran.status, 0, "stderr: {}", ran.stderr);
    assert_eq!(
        types(&ran.events),
        ["start", "model_request", "model_call", "answer", "exit"]
    );
    let start = &ran.events[0];
    assert!(
        start["profile_hash"]
            .as_str()
            .is_some_and(|hash| hash.starts_with("sha256:") && hash.len() == 7 + 64),
        "{start}"
    );
    let build = jakkals::build_info::THIS;
    assert_eq!(start["version"], build.version);
    assert_eq!(start["commit"], json!(build.commit));
    assert_eq!(start["dirty"], json!(build.dirty));
    assert_eq!(start["tools"], json!([]));
    assert_eq!(start["sandbox"], Value::Null);
    assert_eq!(ran.events[3]["text"], "It is empty.");
    let exit = &ran.events[4];
    assert_eq!(exit["reason"], "done");
    assert_eq!(exit["totals"]["cost_nano_usd"], 100_000);
}

#[test]
fn a_call_to_a_tool_not_offered_is_refused_and_the_run_goes_on() {
    let dir = TempDir::new("refused");
    let address = serve(vec![
        (200, tool_call("read")),
        (200, answer("I can't look.")),
    ]);
    let ran = run(&dir, &profile(address, 5), &KEY);

    assert_eq!(ran.status, 0, "stderr: {}", ran.stderr);
    assert_eq!(
        types(&ran.events),
        [
            "start",
            "model_request",
            "model_call",
            "tool_call",
            "model_request",
            "model_call",
            "answer",
            "exit"
        ]
    );
    assert_eq!(ran.events[3]["status"], "refused");
}

#[test]
fn a_local_tool_runs_in_the_working_directory() {
    let dir = TempDir::new("local");
    std::fs::write(dir.0.join("box.txt"), "a cat\n").expect("file is writable");
    let address = serve(vec![(200, tool_call("list")), (200, answer("A cat."))]);
    let text = format!("{}[tools]\nlocal = [\"list\"]\n", profile(address, 5));
    let ran = run(&dir, &text, &KEY);

    assert_eq!(ran.status, 0, "stderr: {}", ran.stderr);
    assert_eq!(ran.events[0]["tools"], json!(["list"]));
    let tool_call = &ran.events[3];
    assert_eq!(tool_call["tool"], "list");
    assert_eq!(tool_call["status"], "ok");
    // The profile and the box: the listing the model got back.
    assert_eq!(tool_call["result_bytes"], "box.txt\nprofile.toml\n".len());
}

#[test]
fn the_shell_runs_an_allowed_command_and_the_start_names_its_sandbox() {
    let dir = TempDir::new("shell");
    let address = serve(vec![
        (200, tool_call_with("shell", r#"{"command":"echo hello"}"#)),
        (
            200,
            tool_call_with("shell", r#"{"command":"rm profile.toml"}"#),
        ),
        (200, answer("Done.")),
    ]);
    let text = format!(
        "{}[tools]\nlocal = [\"shell\"]\nshell_allow = [\"echo\"]\nsandbox = \"none\"\n",
        profile(address, 5)
    );
    let ran = run(&dir, &text, &KEY);

    assert_eq!(ran.status, 0, "stderr: {}", ran.stderr);
    assert_eq!(ran.events[0]["tools"], json!(["shell"]));
    assert_eq!(ran.events[0]["sandbox"], "none");
    assert_eq!(ran.events[3]["status"], "ok");
    assert_eq!(ran.events[3]["result_bytes"], "hello\n".len());
    assert_eq!(ran.events[6]["status"], "refused");
    assert!(dir.0.join("profile.toml").exists());
}

#[test]
fn a_limit_ends_the_run_with_status_3() {
    let dir = TempDir::new("limit");
    let address = serve(vec![(200, tool_call("read"))]);
    let ran = run(&dir, &profile(address, 1), &KEY);

    assert_eq!(ran.status, 3, "stderr: {}", ran.stderr);
    let exit = ran.events.last().expect("an exit event");
    assert_eq!(exit["reason"], "limit");
    assert_eq!(exit["which"], "steps");
}

#[test]
fn a_provider_error_ends_the_run_with_status_4() {
    let dir = TempDir::new("error");
    let address = serve(vec![(500, json!({"error": {"message": "Down"}}))]);
    let ran = run(&dir, &profile(address, 5), &KEY);

    assert_eq!(ran.status, 4, "stderr: {}", ran.stderr);
    let exit = ran.events.last().expect("an exit event");
    assert_eq!(exit["reason"], "error");
    assert_eq!(exit["error"]["kind"], "provider");
    assert_eq!(exit["error"]["provider_error"], "status");
    assert_eq!(exit["error"]["status"], 500);
}

#[test]
fn setup_faults_exit_2_with_no_events() {
    let dir = TempDir::new("setup");
    // Nothing listens here: a run that started would fail on transport,
    // not with status 2.
    let address: SocketAddr = "127.0.0.1:9".parse().expect("an address");
    let cases = [
        (profile(address, 5), vec![], "JAKKALS_TEST_KEY"),
        (
            profile(address, 5),
            vec![("JAKKALS_TEST_KEY", "")],
            "JAKKALS_TEST_KEY",
        ),
        ("[limits]\nsteps = 5\n".to_owned(), KEY.to_vec(), "wall_s"),
        (
            format!(
                "{}[mcp.memory]\nurl = \"http://127.0.0.1:9/mcp\"\ntools = [\"recall\"]\n",
                profile(address, 5)
            ),
            KEY.to_vec(),
            "MCP server `memory`",
        ),
        (
            format!(
                "{}[mcp.memory]\nurl = \"http://127.0.0.1:9/mcp\"\ntools = [\"recall\"]\n\
                 key_env = \"JAKKALS_TEST_MCP_KEY\"\n",
                profile(address, 5)
            ),
            KEY.to_vec(),
            "mcp.memory.key_env",
        ),
    ];
    for (text, env, named) in cases {
        let ran = run(&dir, &text, &env);
        assert_eq!(ran.status, 2, "{text:?}: stderr: {}", ran.stderr);
        assert!(ran.events.is_empty(), "{text:?}: no events");
        assert!(
            ran.stderr.contains(named),
            "{text:?}: {:?} names {named}",
            ran.stderr
        );
    }
    let missing = dir.0.join("no-such-directory");
    let ran = run_in(&dir, &missing, &profile(address, 5), &KEY);
    assert_eq!(ran.status, 2, "stderr: {}", ran.stderr);
    assert!(ran.events.is_empty());
    assert!(ran.stderr.contains("--cwd"), "{:?}", ran.stderr);
}

#[test]
fn a_transcript_records_the_conversation_and_asked_for_reasoning() {
    let dir = TempDir::new("transcript");
    let mut reply = answer("A cat.");
    reply["choices"][0]["message"]["reasoning"] = json!("Check the box.");
    let address = serve(vec![(200, reply)]);
    let path = dir.0.join("transcript.jsonl");
    let extra = [
        OsString::from("--transcript"),
        path.clone().into_os_string(),
        OsString::from("--transcript-reasoning"),
    ];
    let ran = run_with(&dir, &dir.0, &profile(address, 5), &KEY, &extra);
    assert_eq!(ran.status, 0, "stderr: {}", ran.stderr);
    assert_eq!(
        types(&ran.events),
        ["start", "model_request", "model_call", "answer", "exit"]
    );

    let written = std::fs::read_to_string(&path).expect("the transcript exists");
    let lines: Vec<Value> = written
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect();
    let roles: Vec<&str> = lines
        .iter()
        .map(|line| line["role"].as_str().expect("a role"))
        .collect();
    assert_eq!(roles, ["system", "user", "assistant"]);
    assert_eq!(lines[1]["text"], "What is in the box?");
    assert_eq!(lines[2]["text"], "A cat.");
    assert_eq!(lines[2]["reasoning"], "Check the box.");
    assert_eq!(ran.events[0]["transcript"], path.to_str().expect("UTF-8"));
    let mode = std::fs::metadata(&path)
        .expect("exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_bare_transcript_goes_in_the_data_folder_under_a_generated_name() {
    let dir = TempDir::new("transcript-default");
    let address = serve(vec![(200, answer("A cat."))]);
    let data = dir.0.join("data");
    let data_home = data.to_str().expect("UTF-8");
    let env = [KEY[0], ("XDG_DATA_HOME", data_home)];
    // A bare --transcript followed by another option: the option isn't
    // taken as its path.
    let extra = [
        OsString::from("--transcript"),
        OsString::from("--transcript-reasoning"),
    ];
    let ran = run_with(&dir, &dir.0, &profile(address, 5), &env, &extra);
    assert_eq!(ran.status, 0, "stderr: {}", ran.stderr);

    let folder = data.join("jakkals/transcripts");
    let files: Vec<PathBuf> = std::fs::read_dir(&folder)
        .expect("the folder was made")
        .map(|entry| entry.expect("an entry").path())
        .collect();
    let [file] = files.as_slice() else {
        panic!("one transcript, not {files:?}");
    };
    let name = file
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a UTF-8 name");
    // `2026-01-31T14-05-09Z-<pid>.jsonl`: the shape, not the moment.
    let bytes = name.as_bytes();
    assert!(name.ends_with(".jsonl") && bytes.len() > 27, "{name}");
    assert_eq!(
        (bytes[10], bytes[19], bytes[20]),
        (b'T', b'Z', b'-'),
        "{name}"
    );
    assert!(!name.contains(':'), "{name}");
    assert_eq!(ran.events[0]["transcript"], file.to_str().expect("UTF-8"));
    let lines = std::fs::read_to_string(file)
        .expect("readable")
        .lines()
        .count();
    assert_eq!(lines, 3, "system, user, assistant");
    let mode = std::fs::metadata(&folder)
        .expect("exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[test]
fn without_a_transcript_the_start_says_none() {
    let dir = TempDir::new("no-transcript");
    let address = serve(vec![(200, answer("A cat."))]);
    let ran = run(&dir, &profile(address, 5), &KEY);
    assert_eq!(ran.status, 0, "stderr: {}", ran.stderr);
    assert_eq!(ran.events[0]["transcript"], Value::Null);
}

#[test]
fn a_transcript_path_that_exists_or_reasoning_alone_exits_2() {
    let dir = TempDir::new("transcript-setup");
    let address: SocketAddr = "127.0.0.1:9".parse().expect("an address");
    let path = dir.0.join("taken.jsonl");
    std::fs::write(&path, "someone else's\n").expect("writable");

    let extra = [
        OsString::from("--transcript"),
        path.clone().into_os_string(),
    ];
    let ran = run_with(&dir, &dir.0, &profile(address, 5), &KEY, &extra);
    assert_eq!(ran.status, 2, "stderr: {}", ran.stderr);
    assert!(ran.events.is_empty());
    assert!(ran.stderr.contains("--transcript"), "{:?}", ran.stderr);
    assert_eq!(
        std::fs::read_to_string(&path).expect("readable"),
        "someone else's\n",
        "the existing file is left alone"
    );

    let extra = [OsString::from("--transcript-reasoning")];
    let ran = run_with(&dir, &dir.0, &profile(address, 5), &KEY, &extra);
    assert_eq!(ran.status, 2, "stderr: {}", ran.stderr);
    assert!(ran.events.is_empty());
}

/// A fake MCP server offering `recall`, which answers every call with
/// "a note". Streamable HTTP with plain JSON replies, one request per
/// connection.
fn serve_mcp() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback port");
    let address = listener.local_addr().expect("bound address");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            thread::spawn(move || answer_mcp(stream));
        }
    });
    address
}

fn answer_mcp(stream: std::net::TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().expect("stream clones"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let line = line.trim_end().to_ascii_lowercase();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("content-length:") {
            length = value.trim().parse().expect("length is a number");
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let reply = |status: u16, body: &str| {
        format!(
            "HTTP/1.1 {status} Canned\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let text = if !request_line.starts_with("POST") {
        reply(405, "")
    } else {
        let request: Value = serde_json::from_slice(&body).expect("a JSON-RPC body");
        match request.get("id") {
            None => reply(202, ""),
            Some(id) => {
                let result = match request["method"].as_str() {
                    Some("initialize") => json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fake-memory", "version": "0.1.0"}
                    }),
                    Some("tools/list") => json!({"tools": [{
                        "name": "recall",
                        "description": "Recall a note.",
                        "inputSchema": {"type": "object"}
                    }]}),
                    Some("tools/call") => {
                        json!({"content": [{"type": "text", "text": "a note"}]})
                    }
                    other => panic!("the fake MCP server doesn't answer {other:?}"),
                };
                reply(
                    200,
                    &json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
                )
            }
        }
    };
    let mut stream = stream;
    let _ = stream.write_all(text.as_bytes());
}

#[test]
fn an_mcp_tool_is_offered_recorded_and_called() {
    let dir = TempDir::new("mcp");
    let mcp = serve_mcp();
    let address = serve(vec![
        (200, tool_call("memory_recall")),
        (200, answer("A note.")),
    ]);
    let text = format!(
        "{}\n[mcp.memory]\nurl = \"http://{mcp}/mcp\"\ntools = [\"recall\"]\n",
        profile(address, 5)
    );
    let ran = run(&dir, &text, &KEY);

    assert_eq!(ran.status, 0, "stderr: {}", ran.stderr);
    assert_eq!(
        types(&ran.events),
        [
            "start",
            "mcp_server",
            "model_request",
            "model_call",
            "tool_call",
            "model_request",
            "model_call",
            "answer",
            "exit"
        ]
    );
    assert_eq!(ran.events[0]["tools"], json!(["memory_recall"]));
    let server = &ran.events[1];
    assert_eq!(server["server"], "memory");
    assert_eq!(server["server_name"], "fake-memory");
    assert_eq!(server["server_version"], "0.1.0");
    assert_eq!(server["protocol_version"], "2025-06-18");
    assert_eq!(
        server["tools"],
        json!([{
            "name": "memory_recall",
            "description": "Recall a note.",
            "parameters": {"type": "object"}
        }])
    );
    let call = &ran.events[4];
    assert_eq!(call["tool"], "memory_recall");
    assert_eq!(call["status"], "ok");
    assert_eq!(call["result_bytes"], 6);
}
