//! MCP servers against a fake server on 127.0.0.1 speaking MCP's
//! streamable HTTP with plain JSON replies: canned answers out, the
//! requests Jakkals sent checked. No network beyond the loopback.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

use serde_json::json;

use super::*;

/// A request as the fake server read it.
#[derive(Clone, Debug)]
struct Seen {
    http_method: String,
    /// Header names lowercased.
    headers: Vec<(String, String)>,
    /// The JSON-RPC body; `Null` for a request without one.
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn rpc_method(&self) -> Option<&str> {
        self.body["method"].as_str()
    }
}

/// How the fake server answers one JSON-RPC request.
enum Answer {
    Result(Value),
    Error(i64, &'static str),
    /// Answers with an empty result only after `ms`.
    Stall(u64),
    /// An HTTP status with no body, as a server refusing the request.
    Status(u16),
}

/// Serves MCP until the test ends, answering each JSON-RPC request with
/// `answer(method, params)`, and reports every request it reads.
fn serve(
    answer: impl Fn(&str, &Value) -> Answer + Send + Sync + 'static,
) -> (SocketAddr, mpsc::Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback port");
    let address = listener.local_addr().expect("bound address");
    let (seen_sender, seen) = mpsc::channel();
    let answer = Arc::new(answer);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let answer = Arc::clone(&answer);
            let seen_sender = seen_sender.clone();
            thread::spawn(move || {
                // One request per connection: every reply closes it.
                let Some(request) = read_request(&stream) else {
                    return;
                };
                // Reported before answering, so a stalled answer's
                // request is seen while it stalls. The test may be gone.
                let _ = seen_sender.send(request.clone());
                let reply = respond(&request, answer.as_ref());
                let mut stream = stream;
                let _ = stream.write_all(&reply);
            });
        }
    });
    (address, seen)
}

fn respond(request: &Seen, answer: &dyn Fn(&str, &Value) -> Answer) -> Vec<u8> {
    let head = |status: u16, extra: &str, body: &str| {
        format!(
            "HTTP/1.1 {status} Scripted\r\n{extra}content-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    };
    if request.http_method != "POST" {
        // No stream of the server's own, and no session to delete.
        return head(405, "", "");
    }
    let Some(id) = request.body.get("id") else {
        // A notification.
        return head(202, "", "");
    };
    let method = request.rpc_method().expect("a request has a method");
    let reply = match answer(method, &request.body["params"]) {
        Answer::Result(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Answer::Error(code, message) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
        Answer::Stall(ms) => {
            thread::sleep(Duration::from_millis(ms));
            json!({"jsonrpc": "2.0", "id": id, "result": {"content": []}})
        }
        Answer::Status(status) => return head(status, "location: http://127.0.0.1:9/\r\n", ""),
    };
    head(
        200,
        "content-type: application/json\r\nmcp-session-id: session-1\r\n",
        &reply.to_string(),
    )
}

fn read_request(stream: &TcpStream) -> Option<Seen> {
    let mut reader = BufReader::new(stream.try_clone().expect("stream clones"));
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let http_method = request_line.split(' ').next()?.to_owned();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map_or(0, |(_, value)| value.parse().expect("length is a number"));
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Seen {
        http_method,
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

fn initialized() -> Answer {
    Answer::Result(json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "fake-notes", "version": "1.2.3"}
    }))
}

fn tool(name: &str) -> Value {
    json!({
        "name": name,
        "description": format!("The {name} tool."),
        "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}
    })
}

/// A server with the tools `search`, `read_note` and `delete_note`,
/// answering a call with `call(params)`.
fn notes_server(
    call: impl Fn(&Value) -> Answer + Send + Sync + 'static,
) -> (SocketAddr, mpsc::Receiver<Seen>) {
    serve(move |method, params| match method {
        "initialize" => initialized(),
        "tools/list" => Answer::Result(json!({
            "tools": [tool("search"), tool("read_note"), tool("delete_note")]
        })),
        "tools/call" => call(params),
        other => panic!("the fake server doesn't answer {other}"),
    })
}

fn settings(address: SocketAddr, tools: &[&str]) -> McpSettings {
    McpSettings {
        name: "notes".to_owned(),
        url: format!("http://{address}/mcp"),
        tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
        key: None,
        call_timeout_s: CALL_TIMEOUT_S_DEFAULT,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

fn connect(settings: &McpSettings, key: Option<&str>) -> Result<McpServer, McpSetupError> {
    runtime().block_on(McpServer::connect(settings, key))
}

/// Every request the server has read so far.
fn drain(seen: &mpsc::Receiver<Seen>) -> Vec<Seen> {
    seen.try_iter().collect()
}

fn text_result(text: &str) -> Answer {
    Answer::Result(json!({"content": [{"type": "text", "text": text}]}))
}

#[test]
fn connecting_offers_the_listed_tools_in_the_profile_order_and_records_the_server() {
    let (address, seen) = notes_server(|_| text_result("unused"));
    let started = runtime();
    let server = started
        .block_on(McpServer::connect(
            &settings(address, &["read_note", "search"]),
            None,
        ))
        .expect("the server sets up");

    let names: Vec<&str> = server
        .specs()
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert_eq!(names, ["notes_read_note", "notes_search"]);
    assert_eq!(server.specs()[0].description, "The read_note tool.");
    assert_eq!(
        server.specs()[0].parameters,
        json!({"type": "object", "properties": {"q": {"type": "string"}}})
    );
    let record = server.record();
    assert_eq!(record.server, "notes");
    assert_eq!(record.server_name.as_deref(), Some("fake-notes"));
    assert_eq!(record.server_version.as_deref(), Some("1.2.3"));
    assert_eq!(record.protocol_version, "2025-06-18");
    assert_eq!(record.tools, server.specs());
    assert!(server.offers("notes_search"));
    assert!(!server.offers("notes_delete_note"));
    assert!(!server.offers("search"));

    let requests = drain(&seen);
    let initialize = requests
        .iter()
        .find(|request| request.rpc_method() == Some("initialize"))
        .expect("an initialize request");
    assert_eq!(initialize.body["params"]["capabilities"], json!({}));
    assert_eq!(initialize.body["params"]["clientInfo"]["name"], "jakkals");
    assert!(initialize.header("authorization").is_none());
}

#[test]
fn a_key_goes_as_a_bearer_token_by_default() {
    let (address, seen) = notes_server(|_| text_result("unused"));
    let mut settings = settings(address, &["search"]);
    settings.key = Some(KeySettings {
        env: "NOTES_KEY".to_owned(),
        header: "authorization".to_owned(),
    });
    connect(&settings, Some("secret-1")).expect("the server sets up");
    for request in drain(&seen)
        .iter()
        .filter(|request| request.http_method == "POST")
    {
        assert_eq!(request.header("authorization"), Some("Bearer secret-1"));
    }
}

#[test]
fn a_key_goes_as_it_is_in_a_named_header() {
    let (address, seen) = notes_server(|_| text_result("unused"));
    let mut settings = settings(address, &["search"]);
    settings.key = Some(KeySettings {
        env: "NOTES_KEY".to_owned(),
        header: "x-api-key".to_owned(),
    });
    connect(&settings, Some("secret-2")).expect("the server sets up");
    let requests = drain(&seen);
    assert!(!requests.is_empty());
    for request in requests
        .iter()
        .filter(|request| request.http_method == "POST")
    {
        assert_eq!(request.header("x-api-key"), Some("secret-2"));
        assert!(request.header("authorization").is_none());
    }
}

#[test]
fn a_tool_the_server_lacks_stops_setup() {
    let (address, _seen) = notes_server(|_| text_result("unused"));
    let error = connect(&settings(address, &["search", "archive"]), None)
        .err()
        .expect("setup fails");
    assert_eq!(error.server, "notes");
    assert!(matches!(error.problem, SetupProblem::MissingTool(tool) if tool == "archive"));
}

#[test]
fn a_server_refusing_initialization_stops_setup() {
    let (address, _seen) = serve(|method, _| match method {
        "initialize" => Answer::Error(-32001, "Unauthorized"),
        other => panic!("not reached: {other}"),
    });
    let error = connect(&settings(address, &["search"]), None)
        .err()
        .expect("setup fails");
    assert!(matches!(error.problem, SetupProblem::Initialize(_)));
}

#[test]
fn a_redirect_is_not_followed() {
    let (address, seen) = serve(|method, _| match method {
        "initialize" => Answer::Status(307),
        other => panic!("not reached: {other}"),
    });
    let error = connect(&settings(address, &["search"]), None)
        .err()
        .expect("setup fails");
    assert!(matches!(error.problem, SetupProblem::Initialize(_)));
    // One POST: neither followed nor tried again.
    let posts = drain(&seen)
        .into_iter()
        .filter(|request| request.http_method == "POST")
        .count();
    assert_eq!(posts, 1);
}

#[test]
fn no_server_listening_stops_setup() {
    // Bound, then closed: nothing listens there.
    let address = TcpListener::bind("127.0.0.1:0")
        .expect("loopback port")
        .local_addr()
        .expect("bound address");
    let error = connect(&settings(address, &["search"]), None)
        .err()
        .expect("setup fails");
    assert!(matches!(error.problem, SetupProblem::Initialize(_)));
}

#[test]
fn every_page_of_the_tool_list_is_read() {
    let (address, _seen) = serve(|method, params| match method {
        "initialize" => initialized(),
        "tools/list" => match params["cursor"].as_str() {
            None => Answer::Result(json!({"tools": [tool("search")], "nextCursor": "page-2"})),
            Some("page-2") => Answer::Result(json!({"tools": [tool("read_note")]})),
            Some(other) => panic!("no page {other}"),
        },
        other => panic!("not reached: {other}"),
    });
    let server =
        connect(&settings(address, &["read_note", "search"]), None).expect("the server sets up");
    assert_eq!(server.specs().len(), 2);
}

#[test]
fn a_tool_list_paging_past_the_cap_stops_setup() {
    let (address, seen) = serve(|method, _| match method {
        "initialize" => initialized(),
        "tools/list" => Answer::Result(json!({"tools": [], "nextCursor": "again"})),
        other => panic!("not reached: {other}"),
    });
    let error = connect(&settings(address, &["search"]), None)
        .err()
        .expect("setup fails");
    assert!(matches!(error.problem, SetupProblem::TooManyPages));
    let pages = drain(&seen)
        .iter()
        .filter(|request| request.rpc_method() == Some("tools/list"))
        .count();
    assert_eq!(pages, usize::try_from(TOOL_LIST_CAP_PAGES).expect("fits"));
}

/// Sets up the notes server and makes one call to `notes_search`.
fn call_search(
    call: impl Fn(&Value) -> Answer + Send + Sync + 'static,
    arguments: &str,
    deadline_ms: u64,
) -> (ToolOutcome, Vec<Seen>) {
    call_search_timed(call, arguments, deadline_ms, CALL_TIMEOUT_S_DEFAULT)
}

/// As [`call_search`], with the server's own `call_timeout_s`.
fn call_search_timed(
    call: impl Fn(&Value) -> Answer + Send + Sync + 'static,
    arguments: &str,
    deadline_ms: u64,
    call_timeout_s: u32,
) -> (ToolOutcome, Vec<Seen>) {
    let (address, seen) = notes_server(call);
    let runtime = runtime();
    let settings = McpSettings {
        call_timeout_s,
        ..settings(address, &["search"])
    };
    let server = runtime
        .block_on(McpServer::connect(&settings, None))
        .expect("the server sets up");
    let outcome = runtime.block_on(server.call("notes_search", arguments, deadline_ms));
    drop(server);
    // Give the server's threads a moment to report what they read.
    thread::sleep(Duration::from_millis(50));
    (outcome, drain(&seen))
}

#[test]
fn a_call_sends_the_server_its_own_name_and_the_arguments() {
    let (outcome, seen) = call_search(|_| text_result("two notes"), r#"{"q": "jackal"}"#, 5000);
    assert_eq!(outcome, ToolOutcome::Ok("two notes".to_owned()));
    let call = seen
        .iter()
        .find(|request| request.rpc_method() == Some("tools/call"))
        .expect("a call");
    assert_eq!(call.body["params"]["name"], "search");
    assert_eq!(call.body["params"]["arguments"], json!({"q": "jackal"}));
}

#[test]
fn empty_arguments_are_sent_as_an_empty_object() {
    let (outcome, seen) = call_search(|_| text_result("all notes"), "", 5000);
    assert_eq!(outcome, ToolOutcome::Ok("all notes".to_owned()));
    let call = seen
        .iter()
        .find(|request| request.rpc_method() == Some("tools/call"))
        .expect("a call");
    assert_eq!(call.body["params"]["arguments"], json!({}));
}

#[test]
fn arguments_that_are_not_an_object_fail_before_the_server_sees_them() {
    let (outcome, seen) = call_search(|_| panic!("not called"), "[1, 2]", 5000);
    assert_eq!(
        outcome,
        ToolOutcome::Failed(
            Failure::Arguments,
            "the arguments must be a JSON object".to_owned()
        )
    );
    assert!(
        seen.iter()
            .all(|request| request.rpc_method() != Some("tools/call"))
    );
}

#[test]
fn a_result_marked_as_an_error_fails() {
    let (outcome, _) = call_search(
        |_| {
            Answer::Result(json!({
                "content": [{"type": "text", "text": "no such note"}],
                "isError": true
            }))
        },
        "{}",
        5000,
    );
    assert_eq!(
        outcome,
        ToolOutcome::Failed(Failure::IsError, "no such note".to_owned())
    );
}

#[test]
fn an_mcp_error_fails_with_its_code_and_message() {
    let (outcome, _) = call_search(|_| Answer::Error(-32602, "q is required"), "{}", 5000);
    assert_eq!(
        outcome,
        ToolOutcome::Failed(
            Failure::McpError { code: -32602 },
            "MCP error -32602: q is required".to_owned()
        )
    );
}

#[test]
fn a_call_past_the_deadline_fails_and_is_cancelled() {
    let (outcome, seen) = call_search(|_| Answer::Stall(1000), "{}", 200);
    assert_eq!(
        outcome,
        ToolOutcome::Failed(
            Failure::Deadline,
            "jakkals: the call ran past its 200 ms and was cancelled".to_owned()
        )
    );
    let call_id = seen
        .iter()
        .find(|request| request.rpc_method() == Some("tools/call"))
        .map(|request| request.body["id"].clone())
        .expect("a call");
    // The cancellation arrives on its own connection while the stalled
    // answer is still being held.
    let cancelled = seen.iter().any(|request| {
        request.rpc_method() == Some("notifications/cancelled")
            && request.body["params"]["requestId"] == call_id
    });
    assert!(cancelled, "the server was told the call is cancelled");
}

#[test]
fn a_call_past_its_own_timeout_fails_as_a_timeout() {
    let (outcome, _) = call_search_timed(|_| Answer::Stall(3000), "{}", 5000, 1);
    assert_eq!(
        outcome,
        ToolOutcome::Failed(
            Failure::Timeout,
            "jakkals: the call ran past its 1000 ms and was cancelled".to_owned()
        )
    );
}

fn result(value: Value) -> CallToolResult {
    serde_json::from_value(value).expect("a call result")
}

#[test]
fn text_parts_are_joined_and_other_parts_are_marked_left_out() {
    let outcome = outcome(result(json!({"content": [
        {"type": "text", "text": "first"},
        {"type": "image", "data": "AAAA", "mimeType": "image/png"},
        {"type": "text", "text": "second"}
    ]})));
    assert_eq!(
        outcome,
        ToolOutcome::Ok("first\n[jakkals: image left out]\nsecond".to_owned())
    );
}

#[test]
fn structured_content_is_returned_only_without_text() {
    let structured = json!({"notes": 2});
    let with_text = outcome(result(json!({
        "content": [{"type": "text", "text": "{\"notes\":2}"}],
        "structuredContent": structured
    })));
    assert_eq!(with_text, ToolOutcome::Ok("{\"notes\":2}".to_owned()));
    let without_text = outcome(result(json!({
        "content": [],
        "structuredContent": structured
    })));
    assert_eq!(without_text, ToolOutcome::Ok("{\"notes\":2}".to_owned()));
}

#[test]
fn an_empty_result_is_empty_text() {
    assert_eq!(
        outcome(result(json!({"content": []}))),
        ToolOutcome::Ok(String::new())
    );
}
