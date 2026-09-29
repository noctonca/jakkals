//! The HTTP provider against a fake server on 127.0.0.1: canned
//! responses out, the exact request and the typed result checked. No
//! network beyond the loopback, no key.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc;
use std::thread;

use serde_json::json;

use super::*;

/// A request as the fake server read it.
struct Seen {
    request_line: String,
    /// Header names lowercased.
    headers: Vec<(String, String)>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// What the fake server does with one connection.
enum Script {
    Respond {
        status: u16,
        body: Vec<u8>,
    },
    /// Reads the request and answers only after `ms`.
    Stall {
        ms: u64,
    },
}

fn ok(body: &Value) -> Script {
    Script::Respond {
        status: 200,
        body: serde_json::to_vec(body).expect("body serializes"),
    }
}

/// Serves one scripted response per connection, in order, and reports
/// each request it read.
fn serve(scripts: Vec<Script>) -> (SocketAddr, mpsc::Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback port");
    let address = listener.local_addr().expect("bound address");
    let (seen_sender, seen) = mpsc::channel();
    thread::spawn(move || {
        for script in scripts {
            let (stream, _) = listener.accept().expect("a connection");
            let mut reader = BufReader::new(stream.try_clone().expect("stream clones"));
            let seen = read_request(&mut reader);
            // The test may be gone already when a request fails as planned.
            let _ = seen_sender.send(seen);
            let mut stream = stream;
            let (status, body) = match script {
                Script::Respond { status, body } => (status, body),
                Script::Stall { ms } => {
                    thread::sleep(Duration::from_millis(ms));
                    (200, b"{}".to_vec())
                }
            };
            let head = format!(
                "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nlocation: http://127.0.0.1:9/elsewhere\r\n\
                 connection: close\r\n\r\n",
                body.len()
            );
            // A client that gave up has closed its end.
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (address, seen)
}

fn read_request(reader: &mut BufReader<std::net::TcpStream>) -> Seen {
    let mut request_line = String::new();
    reader.read_line(&mut request_line).expect("request line");
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("header line");
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').expect("header has a colon");
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map(|(_, value)| value.parse().expect("length is a number"))
        .expect("request has a content-length");
    let mut body = vec![0; length];
    reader.read_exact(&mut body).expect("request body");
    Seen {
        request_line: request_line.trim_end().to_owned(),
        headers,
        body: serde_json::from_slice(&body).expect("request body is JSON"),
    }
}

fn provider(address: SocketAddr, api_key: Option<&str>, params: Value) -> HttpProvider {
    let Value::Object(params) = params else {
        panic!("params is an object");
    };
    HttpProvider::new(HttpConfig {
        base_url: format!("http://{address}/api/v1/"),
        api_key: api_key.map(str::to_owned),
        params,
    })
    .expect("config is valid")
}

const DEADLINE_MS: u64 = 5_000;

async fn complete(
    provider: &mut HttpProvider,
    messages: &[Message],
    tools: &[ToolSpec],
    deadline_ms: u64,
) -> Result<Reply, ProviderError> {
    provider
        .complete(Request {
            model: "test/model",
            messages,
            tools,
            deadline_ms,
        })
        .await
}

fn user(text: &str) -> Vec<Message> {
    vec![Message::User {
        text: text.to_owned(),
    }]
}

/// A plain answer with the usage a local server reports: no cost, no
/// provider.
fn local_answer() -> Value {
    json!({
        "id": "chatcmpl-1",
        "model": "test/model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "It is empty."},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16}
    })
}

#[tokio::test]
async fn request_is_the_wire_format() {
    let (address, seen) = serve(vec![ok(&local_answer())]);
    let mut provider = provider(
        address,
        Some("test-key"),
        json!({"temperature": 0, "max_tokens": 100}),
    );
    let messages = vec![
        Message::System {
            text: "You are a test.".to_owned(),
        },
        Message::User {
            text: "What is in the box?".to_owned(),
        },
        Message::Assistant {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call-0".to_owned(),
                name: "read".to_owned(),
                arguments: r#"{"path":"box"}"#.to_owned(),
            }],
        },
        Message::Tool {
            call_id: "call-0".to_owned(),
            text: "nothing".to_owned(),
        },
        Message::Assistant {
            text: Some("Let me look again.".to_owned()),
            tool_calls: Vec::new(),
        },
    ];
    let tools = vec![ToolSpec {
        name: "read".to_owned(),
        description: "Read a file.".to_owned(),
        parameters: json!({"type": "object"}),
    }];
    complete(&mut provider, &messages, &tools, DEADLINE_MS)
        .await
        .expect("the reply parses");

    let seen = seen.recv().expect("the server saw the request");
    assert_eq!(seen.request_line, "POST /api/v1/chat/completions HTTP/1.1");
    assert_eq!(seen.header("authorization"), Some("Bearer test-key"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(
        seen.header("user-agent"),
        Some(concat!("jakkals/", env!("CARGO_PKG_VERSION")))
    );
    assert_eq!(
        seen.body,
        json!({
            "model": "test/model",
            "messages": [
                {"role": "system", "content": "You are a test."},
                {"role": "user", "content": "What is in the box?"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call-0",
                    "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":\"box\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call-0", "content": "nothing"},
                {"role": "assistant", "content": "Let me look again."}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "Read a file.",
                    "parameters": {"type": "object"}
                }
            }],
            "temperature": 0,
            "max_tokens": 100
        })
    );
}

#[tokio::test]
async fn no_key_sends_no_authorization_and_no_tools_sends_no_tool_list() {
    let (address, seen) = serve(vec![ok(&local_answer())]);
    let mut provider = provider(address, None, json!({}));
    complete(&mut provider, &user("Hi."), &[], DEADLINE_MS)
        .await
        .expect("the reply parses");

    let seen = seen.recv().expect("the server saw the request");
    assert_eq!(seen.header("authorization"), None);
    assert_eq!(
        seen.body,
        json!({"model": "test/model", "messages": [{"role": "user", "content": "Hi."}]})
    );
}

#[tokio::test]
async fn openrouter_reply_keeps_cost_provider_and_generation() {
    let reply = json!({
        "id": "gen-1",
        "provider": "Test Upstream",
        "model": "test/served-model",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "tool_calls",
            "native_finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "call-a", "index": 0, "type": "function",
                     "function": {"name": "read", "arguments": "{\"path\":\"a\"}"}},
                    {"id": "call-b", "index": 1, "type": "function",
                     "function": {"name": "list", "arguments": "{}"}}
                ]
            }
        }],
        "usage": {
            "prompt_tokens": 1200,
            "completion_tokens": 30,
            "total_tokens": 1230,
            "cost": 0.000123456,
            "prompt_tokens_details": {"cached_tokens": 1024},
            "completion_tokens_details": {"reasoning_tokens": 0}
        }
    });
    let (address, _seen) = serve(vec![ok(&reply)]);
    let mut provider = provider(address, Some("test-key"), json!({}));
    let reply = complete(&mut provider, &user("Look."), &[], DEADLINE_MS)
        .await
        .expect("the reply parses");

    assert_eq!(
        reply,
        Reply {
            text: None,
            tool_calls: vec![
                ToolCall {
                    id: "call-a".to_owned(),
                    name: "read".to_owned(),
                    arguments: r#"{"path":"a"}"#.to_owned(),
                },
                ToolCall {
                    id: "call-b".to_owned(),
                    name: "list".to_owned(),
                    arguments: "{}".to_owned(),
                },
            ],
            usage: Usage {
                prompt_tokens: 1200,
                completion_tokens: 30,
                cached_tokens: Some(1024),
                cost_nano_usd: Some(123_456),
            },
            generation_id: Some("gen-1".to_owned()),
            model: Some("test/served-model".to_owned()),
            provider: Some("Test Upstream".to_owned()),
            finish_reason: Some("tool_calls".to_owned()),
        }
    );
}

#[tokio::test]
async fn local_reply_without_cost_is_none_not_zero() {
    let (address, _seen) = serve(vec![ok(&local_answer())]);
    let mut provider = provider(address, None, json!({}));
    let reply = complete(&mut provider, &user("Look."), &[], DEADLINE_MS)
        .await
        .expect("the reply parses");

    assert_eq!(reply.text.as_deref(), Some("It is empty."));
    assert!(reply.tool_calls.is_empty());
    assert_eq!(reply.usage.cost_nano_usd, None);
    assert_eq!(reply.usage.cached_tokens, None);
    assert_eq!(reply.provider, None);
    assert_eq!(reply.generation_id.as_deref(), Some("chatcmpl-1"));
}

#[tokio::test]
async fn error_status_carries_status_and_body() {
    let body = br#"{"error":{"code":429,"message":"Rate limited"}}"#;
    let (address, _seen) = serve(vec![Script::Respond {
        status: 429,
        body: body.to_vec(),
    }]);
    let mut provider = provider(address, Some("test-key"), json!({}));
    let error = complete(&mut provider, &user("Hi."), &[], DEADLINE_MS)
        .await
        .expect_err("a 429 is an error");

    assert_eq!(
        error,
        ProviderError::Status {
            status: 429,
            body: String::from_utf8(body.to_vec()).expect("body is UTF-8"),
        }
    );
}

#[tokio::test]
async fn long_error_body_is_cut_and_marked() {
    let body = "x".repeat(ERROR_BODY_CAP_BYTES + 10);
    let (address, _seen) = serve(vec![Script::Respond {
        status: 502,
        body: body.into_bytes(),
    }]);
    let mut provider = provider(address, None, json!({}));
    let error = complete(&mut provider, &user("Hi."), &[], DEADLINE_MS)
        .await
        .expect_err("a 502 is an error");

    let ProviderError::Status { status, body } = error else {
        panic!("a status error, not {error:?}");
    };
    assert_eq!(status, 502);
    let expected = format!(
        "{}\n[jakkals: body cut at {ERROR_BODY_CAP_BYTES} bytes]",
        "x".repeat(ERROR_BODY_CAP_BYTES)
    );
    assert_eq!(body, expected);
}

#[tokio::test]
async fn redirect_is_not_followed() {
    let (address, _seen) = serve(vec![Script::Respond {
        status: 307,
        body: Vec::new(),
    }]);
    let mut provider = provider(address, None, json!({}));
    let error = complete(&mut provider, &user("Hi."), &[], DEADLINE_MS)
        .await
        .expect_err("a redirect is an error");

    assert_eq!(
        error,
        ProviderError::Status {
            status: 307,
            body: String::new(),
        }
    );
}

/// Each 2xx body that isn't a completion, and the start of the detail
/// it must produce.
#[tokio::test]
async fn malformed_replies_are_typed() {
    let with_usage = |choices: Value| {
        json!({
            "choices": choices,
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })
    };
    let answer_choice = json!({"message": {"content": "Hi."}, "finish_reason": "stop"});
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (b"not json".to_vec(), "reply: "),
        (
            serde_json::to_vec(&json!({"error": {"code": 502, "message": "Upstream failed"}}))
                .expect("serializes"),
            r#"error object in a 2xx reply: {"code":502"#,
        ),
        (
            serde_json::to_vec(&with_usage(json!([
                {"error": {"code": 502}, "finish_reason": "error"}
            ])))
            .expect("serializes"),
            "error object in a 2xx reply's choice: ",
        ),
        (
            serde_json::to_vec(&with_usage(json!([]))).expect("serializes"),
            "reply has 0 choices, not 1",
        ),
        (
            serde_json::to_vec(&with_usage(json!([
                answer_choice.clone(),
                answer_choice.clone()
            ])))
            .expect("serializes"),
            "reply has 2 choices, not 1",
        ),
        (
            serde_json::to_vec(&json!({"choices": [answer_choice.clone()]})).expect("serializes"),
            "reply has no usage",
        ),
        (
            serde_json::to_vec(&json!({
                "choices": [answer_choice.clone()],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "cost": "0.1"}
            }))
            .expect("serializes"),
            r#"usage.cost is not a cost: "0.1""#,
        ),
        (
            serde_json::to_vec(&json!({
                "choices": [answer_choice.clone()],
                "usage": {"prompt_tokens": -1, "completion_tokens": 1}
            }))
            .expect("serializes"),
            "reply: ",
        ),
    ];
    let count = cases.len();
    let (address, _seen) = serve(
        cases
            .iter()
            .map(|(body, _)| Script::Respond {
                status: 200,
                body: body.clone(),
            })
            .collect(),
    );
    let mut provider = provider(address, None, json!({}));
    for (index, (_, expected)) in cases.iter().enumerate() {
        let error = complete(&mut provider, &user("Hi."), &[], DEADLINE_MS)
            .await
            .expect_err("a malformed reply is an error");
        let ProviderError::Malformed { detail } = &error else {
            panic!("case {index}: malformed, not {error:?}");
        };
        assert!(
            detail.starts_with(expected),
            "case {index}: {detail:?} starts with {expected:?}"
        );
    }
    assert_eq!(count, 8);
}

#[tokio::test]
async fn reply_past_the_cap_is_malformed() {
    let (address, _seen) = serve(vec![Script::Respond {
        status: 200,
        body: vec![b' '; REPLY_CAP_BYTES + 1],
    }]);
    let mut provider = provider(address, None, json!({}));
    let error = complete(&mut provider, &user("Hi."), &[], DEADLINE_MS)
        .await
        .expect_err("an oversized reply is an error");

    assert_eq!(
        error,
        malformed(format!("reply body passed {REPLY_CAP_BYTES} bytes"))
    );
}

#[tokio::test]
async fn deadline_passing_mid_call_is_deadline() {
    let (address, _seen) = serve(vec![Script::Stall { ms: 2_000 }]);
    let mut provider = provider(address, None, json!({}));
    let error = complete(&mut provider, &user("Hi."), &[], 100)
        .await
        .expect_err("the call outlives its deadline");

    assert_eq!(error, ProviderError::Deadline);
}

#[tokio::test]
async fn nothing_listening_is_transport() {
    // Bind and drop, so the port is free and refuses connections.
    let address = TcpListener::bind("127.0.0.1:0")
        .expect("loopback port")
        .local_addr()
        .expect("bound address");
    let mut provider = provider(address, None, json!({}));
    let error = complete(&mut provider, &user("Hi."), &[], DEADLINE_MS)
        .await
        .expect_err("no server, no reply");

    assert!(
        matches!(error, ProviderError::Transport { .. }),
        "transport, not {error:?}"
    );
}

#[test]
fn config_refuses_reserved_params_and_bad_urls() {
    let config = |base_url: &str, params: Map<String, Value>| {
        HttpProvider::new(HttpConfig {
            base_url: base_url.to_owned(),
            api_key: None,
            params,
        })
    };
    for name in RESERVED_PARAMS {
        assert_eq!(
            config(
                OPENROUTER_BASE_URL,
                Map::from_iter([(name.to_owned(), json!(true))])
            )
            .err(),
            Some(ConfigError::ReservedParam {
                name: name.to_owned()
            })
        );
    }
    for url in ["openrouter.ai/api/v1", "ftp://example.com", ""] {
        assert_eq!(
            config(url, Map::new()).err(),
            Some(ConfigError::BaseUrl {
                url: url.to_owned()
            })
        );
    }
    let temperature = Map::from_iter([("temperature".to_owned(), json!(0.2))]);
    assert!(config(OPENROUTER_BASE_URL, temperature).is_ok());
}
