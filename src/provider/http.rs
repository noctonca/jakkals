//! The provider for OpenAI-compatible chat completion servers
//! (OpenRouter, llama-server, LM Studio): our types to the wire and
//! back. One HTTP request per model call; no retries, no redirects.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value};

use crate::conversation::{Message, ToolCall, ToolSpec};
use crate::money;
use crate::provider::{Provider, ProviderError, Reply, Request, Usage};

/// The profile's default for `provider.base_url`.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Request body fields Jakkals sets itself, which `provider.params` may
/// not override: the conversation is the loop's, and `stream` would
/// change the reply's format.
pub(crate) const RESERVED_PARAMS: [&str; 4] = ["model", "messages", "tools", "stream"];

/// The most reply body read. A guard against a runaway server, far
/// above any real reply: a completion's body is its text plus a little
/// JSON, and a 100,000-token reply is well under 1 MiB.
const REPLY_CAP_BYTES: usize = 16 * 1024 * 1024;

/// The most of an error status's body kept in the event. Error bodies
/// are a short JSON object; this keeps a server's HTML error page from
/// swamping the event line.
const ERROR_BODY_CAP_BYTES: usize = 16 * 1024;

pub struct HttpConfig {
    /// The API root; requests go to `{base_url}/chat/completions`.
    pub base_url: String,
    /// Sent as a bearer token; `None` for a server that needs no key.
    pub api_key: Option<String>,
    /// Extra request body fields (temperature, max tokens), passed
    /// through as given.
    pub params: Map<String, Value>,
}

/// Why an [`HttpProvider`] can't be built from its configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The base URL isn't an http or https URL.
    BaseUrl { url: String },
    /// A param names a field Jakkals sets itself.
    ReservedParam { name: String },
    /// The HTTP client couldn't be built (its TLS setup failed).
    Client { detail: String },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BaseUrl { url } => write!(formatter, "`{url}` is not an http or https URL"),
            Self::ReservedParam { name } => {
                write!(formatter, "params may not set `{name}`, which the run sets")
            }
            Self::Client { detail } => {
                write!(formatter, "the HTTP client failed to build: {detail}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

pub struct HttpProvider {
    client: reqwest::Client,
    url: reqwest::Url,
    api_key: Option<String>,
    params: Map<String, Value>,
}

impl HttpProvider {
    pub fn new(config: HttpConfig) -> Result<Self, ConfigError> {
        let url = chat_url(&config.base_url).ok_or_else(|| ConfigError::BaseUrl {
            url: config.base_url.clone(),
        })?;
        if let Some(name) = RESERVED_PARAMS
            .iter()
            .find(|name| config.params.contains_key(**name))
        {
            return Err(ConfigError::ReservedParam {
                name: (*name).to_owned(),
            });
        }
        let client = reqwest::Client::builder()
            .user_agent(concat!("jakkals/", env!("CARGO_PKG_VERSION")))
            // A retry is a second request no event records, and a
            // redirected POST is a request to a server the profile
            // doesn't name: both off, so a redirect shows as its status.
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| ConfigError::Client {
                detail: error_chain(&error),
            })?;
        Ok(Self {
            client,
            url,
            api_key: config.api_key,
            params: config.params,
        })
    }
}

impl Provider for HttpProvider {
    async fn complete(&mut self, request: Request<'_>) -> Result<Reply, ProviderError> {
        let body = serde_json::to_vec(&WireRequest {
            model: request.model,
            messages: request.messages.iter().map(WireMessage::from).collect(),
            tools: request.tools.iter().map(WireTool::from).collect(),
            params: &self.params,
        })
        .expect("request serializes");
        let mut builder = self
            .client
            .post(self.url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            // Covers connecting, sending and reading the whole body.
            .timeout(Duration::from_millis(request.deadline_ms))
            .body(body);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        let mut response = builder.send().await.map_err(transport)?;
        let status = response.status();

        let (body, over_cap) = read_capped(&mut response, REPLY_CAP_BYTES).await?;
        if !status.is_success() {
            return Err(ProviderError::Status {
                status: status.as_u16(),
                body: cut_body(&body, over_cap),
            });
        }
        if over_cap {
            return Err(malformed(format!(
                "reply body passed {REPLY_CAP_BYTES} bytes"
            )));
        }
        parse_reply(&body)
    }
}

/// Where chat completions go under `base_url`; `None` unless it is an
/// http or https URL.
pub(crate) fn chat_url(base_url: &str) -> Option<reqwest::Url> {
    let base_url = base_url.trim_end_matches('/');
    reqwest::Url::parse(&format!("{base_url}/chat/completions"))
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
}

/// Reads the body up to `cap` bytes; the flag says there was more.
async fn read_capped(
    response: &mut reqwest::Response,
    cap: usize,
) -> Result<(Vec<u8>, bool), ProviderError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        let room = cap - body.len();
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

/// An error status's body as the event carries it: lossy UTF-8, cut to
/// [`ERROR_BODY_CAP_BYTES`] and marked when cut.
fn cut_body(body: &[u8], over_cap: bool) -> String {
    let text = String::from_utf8_lossy(body);
    if text.len() <= ERROR_BODY_CAP_BYTES && !over_cap {
        return text.into_owned();
    }
    let mut end = ERROR_BODY_CAP_BYTES.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[jakkals: body cut at {end} bytes]", &text[..end])
}

fn transport(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::Deadline
    } else {
        ProviderError::Transport {
            detail: error_chain(&error),
        }
    }
}

/// The error and its sources, which is where reqwest keeps the cause.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut detail = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        detail.push_str(": ");
        detail.push_str(&cause.to_string());
        source = cause.source();
    }
    detail
}

fn malformed(detail: String) -> ProviderError {
    ProviderError::Malformed { detail }
}

fn parse_reply(body: &[u8]) -> Result<Reply, ProviderError> {
    let wire: WireReply =
        serde_json::from_slice(body).map_err(|error| malformed(format!("reply: {error}")))?;
    // OpenRouter can answer 200 with an error object when the upstream
    // fails after the status was sent.
    if let Some(error) = &wire.error {
        return Err(malformed(format!("error object in a 2xx reply: {error}")));
    }
    let [choice] = <[WireChoice; 1]>::try_from(wire.choices)
        .map_err(|choices| malformed(format!("reply has {} choices, not 1", choices.len())))?;
    if let Some(error) = &choice.error {
        return Err(malformed(format!(
            "error object in a 2xx reply's choice: {error}"
        )));
    }
    let message = choice
        .message
        .ok_or_else(|| malformed("reply's choice has no message".to_owned()))?;
    let usage = wire
        .usage
        .ok_or_else(|| malformed("reply has no usage".to_owned()))?;
    let cost_nano_usd = match &usage.cost {
        None => None,
        Some(cost) => Some(
            money::nano_usd(cost.get())
                .ok_or_else(|| malformed(format!("usage.cost is not a cost: {cost}")))?,
        ),
    };
    let tool_calls = message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|call| ToolCall {
            id: call.id,
            name: call.function.name,
            arguments: call.function.arguments,
        })
        .collect();
    Ok(Reply {
        text: message.content,
        tool_calls,
        usage: Usage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            cached_tokens: usage
                .prompt_tokens_details
                .and_then(|details| details.cached_tokens),
            reasoning_tokens: usage
                .completion_tokens_details
                .and_then(|details| details.reasoning_tokens),
            cost_nano_usd,
        },
        generation_id: wire.id,
        model: wire.model,
        provider: wire.provider,
        finish_reason: choice.finish_reason,
    })
}

// The wire format. Field names are the API's.

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    // Some servers refuse an empty tool list.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(flatten)]
    params: &'a Map<String, Value>,
}

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum WireMessage<'a> {
    System {
        content: &'a str,
    },
    User {
        content: &'a str,
    },
    Assistant {
        content: Option<&'a str>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<WireToolCall<'a>>,
    },
    Tool {
        tool_call_id: &'a str,
        content: &'a str,
    },
}

impl<'a> From<&'a Message> for WireMessage<'a> {
    fn from(message: &'a Message) -> Self {
        match message {
            Message::System { text } => Self::System { content: text },
            Message::User { text } => Self::User { content: text },
            Message::Assistant { text, tool_calls } => Self::Assistant {
                content: text.as_deref(),
                tool_calls: tool_calls
                    .iter()
                    .map(|call| WireToolCall {
                        id: &call.id,
                        kind: "function",
                        function: WireFunction {
                            name: &call.name,
                            arguments: &call.arguments,
                        },
                    })
                    .collect(),
            },
            Message::Tool { call_id, text } => Self::Tool {
                tool_call_id: call_id,
                content: text,
            },
        }
    }
}

#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction<'a>,
}

#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: &'a ToolSpec,
}

impl<'a> From<&'a ToolSpec> for WireTool<'a> {
    fn from(spec: &'a ToolSpec) -> Self {
        Self {
            kind: "function",
            function: spec,
        }
    }
}

#[derive(Deserialize)]
struct WireReply {
    /// OpenRouter's generation id; a local server's completion id.
    id: Option<String>,
    model: Option<String>,
    /// OpenRouter's upstream provider.
    provider: Option<String>,
    #[serde(default)]
    choices: Vec<WireChoice>,
    usage: Option<WireUsage>,
    error: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct WireChoice {
    message: Option<WireReplyMessage>,
    finish_reason: Option<String>,
    error: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct WireReplyMessage {
    content: Option<String>,
    tool_calls: Option<Vec<WireReplyToolCall>>,
}

#[derive(Deserialize)]
struct WireReplyToolCall {
    id: String,
    function: WireReplyFunction,
}

#[derive(Deserialize)]
struct WireReplyFunction {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    prompt_tokens_details: Option<WirePromptDetails>,
    completion_tokens_details: Option<WireCompletionDetails>,
    /// US dollars, kept as the literal the server wrote.
    cost: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct WirePromptDetails {
    cached_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct WireCompletionDetails {
    reasoning_tokens: Option<u32>,
}

#[cfg(test)]
mod tests;
