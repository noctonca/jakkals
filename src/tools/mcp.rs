//! MCP servers named in the profile, reached over MCP's streamable HTTP
//! transport through `rmcp`. See "MCP servers" in docs/ARCHITECTURE.md
//! for what is offered, what a call returns, and what `rmcp` would do on
//! its own that is turned off here.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::ClientHandler;
#[allow(deprecated)] // `ListRootsResult`: see `Client::list_roots`.
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig,
    ClientRequest, ContentBlock, ElicitRequestParams, ElicitResult, ElicitationCreateRequestMethod,
    ErrorData, Implementation, ListRootsRequestMethod, ListRootsResult, ListToolsRequest,
    PaginatedRequestParams, ServerResult,
};
use rmcp::service::{PeerRequestOptions, RequestContext, RoleClient, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::common::client_side_sse::NeverRetry;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::conversation::ToolSpec;
use crate::tools::ToolOutcome;

/// The default for `mcp.<name>.call_timeout_s`: a choice, twice the
/// shell's, since a server may search or fetch on the model's behalf;
/// a server still busy after a minute is better treated as hung.
pub const CALL_TIMEOUT_S_DEFAULT: u32 = 60;

/// Longest a server's name may be: a choice, so a prefixed tool name
/// keeps most of [`TOOL_NAME_CAP_BYTES`] for the tool's own name.
pub const SERVER_NAME_CAP_BYTES: usize = 16;

/// Longest tool name a provider takes: OpenAI's function-name limit,
/// which OpenAI-compatible servers follow.
pub const TOOL_NAME_CAP_BYTES: usize = 64;

/// Headers the transport sets itself, which a key may not take the
/// place of: MCP's streamable HTTP transport and HTTP's own.
pub const RESERVED_HEADERS: [&str; 5] = [
    "accept",
    "content-type",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

/// Longest setting up one server may take (connecting, initializing,
/// reading its tool list): a choice, long for a server that answers,
/// short beside a run.
const SETUP_TIMEOUT_MS: u64 = 10_000;

/// Most pages of a tool list read: a choice. A server's list fits in
/// one page or a few; more is a server paging forever.
const TOOL_LIST_CAP_PAGES: u32 = 16;

/// Longest sending MCP's cancellation notice may take once a call has
/// run out of time: a choice, set here rather than left to `rmcp`'s
/// default of 5 s, since it adds to the call's time past its timeout.
const CANCEL_SEND_TIMEOUT_MS: u64 = 2_000;

/// Largest server-sent event taken: the same 1 MiB a shell command's
/// stream is kept to, far past any tool result worth reading.
const SSE_EVENT_CAP_BYTES: usize = 1024 * 1024;

/// One MCP server as the profile names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpSettings {
    /// The profile's name for it, which prefixes its tools' names.
    pub name: String,
    pub url: String,
    /// The server's tools offered, by the server's names, in order.
    pub tools: Vec<String>,
    pub key: Option<KeySettings>,
    pub call_timeout_s: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySettings {
    /// The environment variable holding the key.
    pub env: String,
    /// The header it goes in, lowercased; `authorization` sends it as a
    /// bearer token.
    pub header: String,
}

impl McpSettings {
    /// The name the model sees for one of the server's tools.
    pub fn offered_name(server: &str, tool: &str) -> String {
        format!("{server}_{tool}")
    }
}

/// What a run records about one server: the `mcp_server` event.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct McpServerRecord {
    /// The profile's name for it.
    pub server: String,
    /// The name and version the server reports, where it does.
    pub server_name: Option<String>,
    pub server_version: Option<String>,
    pub protocol_version: String,
    pub setup_ms: u64,
    /// Each offered tool exactly as the model sees it.
    pub tools: Vec<ToolSpec>,
}

/// Why a server couldn't be set up. The run doesn't start.
#[derive(Debug)]
pub struct McpSetupError {
    pub server: String,
    pub problem: SetupProblem,
}

#[derive(Debug)]
pub enum SetupProblem {
    /// Setting up took longer than [`SETUP_TIMEOUT_MS`].
    Timeout,
    /// The HTTP client couldn't be built.
    Client(String),
    /// A header value the key can't be sent in.
    Key,
    /// Connecting or MCP's initialization failed.
    Initialize(String),
    /// The tool list couldn't be read.
    ListTools(String),
    /// The tool list ran past [`TOOL_LIST_CAP_PAGES`].
    TooManyPages,
    /// A tool the profile lists isn't on the server.
    MissingTool(String),
}

impl fmt::Display for McpSetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "MCP server `{}`: ", self.server)?;
        match &self.problem {
            SetupProblem::Timeout => {
                write!(formatter, "not set up within {} s", SETUP_TIMEOUT_MS / 1000)
            }
            SetupProblem::Client(detail) => write!(formatter, "the HTTP client: {detail}"),
            SetupProblem::Key => write!(formatter, "the key can't be sent in an HTTP header"),
            SetupProblem::Initialize(detail) => write!(formatter, "initializing: {detail}"),
            SetupProblem::ListTools(detail) => write!(formatter, "listing tools: {detail}"),
            SetupProblem::TooManyPages => write!(
                formatter,
                "its tool list runs past {TOOL_LIST_CAP_PAGES} pages"
            ),
            SetupProblem::MissingTool(tool) => {
                write!(formatter, "has no tool `{tool}`, which the profile lists")
            }
        }
    }
}

impl std::error::Error for McpSetupError {}

/// Jakkals as an MCP client: it declares no capabilities and answers
/// every request a server makes with method-not-found. `rmcp`'s own
/// defaults would answer a roots request with an empty list and decline
/// an elicitation, each an answer no event would show.
struct Client;

impl ClientHandler for Client {
    // Roots are deprecated in MCP, but a server may still ask for them.
    #[allow(deprecated)]
    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        Err(ErrorData::method_not_found::<ListRootsRequestMethod>())
    }

    async fn create_elicitation(
        &self,
        _request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        Err(ErrorData::method_not_found::<ElicitationCreateRequestMethod>())
    }

    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("jakkals", env!("CARGO_PKG_VERSION")),
        )
    }
}

/// One server, set up, with the tools the profile offers from it.
pub struct McpServer {
    service: RunningService<RoleClient, Client>,
    /// The offered tools: the name the model sees, and the server's.
    tools: Vec<(String, String)>,
    call_timeout_ms: u64,
    record: McpServerRecord,
}

impl McpServer {
    /// Connects, initializes and reads the tool list, within
    /// [`SETUP_TIMEOUT_MS`]. `key` is the key's value, read from the
    /// variable `settings.key` names.
    pub async fn connect(settings: &McpSettings, key: Option<&str>) -> Result<Self, McpSetupError> {
        assert_eq!(
            settings.key.is_some(),
            key.is_some(),
            "a key is given exactly when the profile names one"
        );
        assert!(!settings.tools.is_empty(), "the profile offers a tool");
        let failed = |problem| McpSetupError {
            server: settings.name.clone(),
            problem,
        };
        let started = Instant::now();
        let setup = tokio::time::timeout(
            Duration::from_millis(SETUP_TIMEOUT_MS),
            Self::set_up(settings, key),
        );
        let (service, listed) = setup
            .await
            .map_err(|_| failed(SetupProblem::Timeout))?
            .map_err(failed)?;

        let mut tools = Vec::with_capacity(settings.tools.len());
        let mut specs = Vec::with_capacity(settings.tools.len());
        for wanted in &settings.tools {
            let tool = listed
                .iter()
                .find(|tool| tool.name == *wanted)
                .ok_or_else(|| failed(SetupProblem::MissingTool(wanted.clone())))?;
            let name = McpSettings::offered_name(&settings.name, wanted);
            specs.push(ToolSpec {
                name: name.clone(),
                description: tool.description.as_deref().unwrap_or_default().to_owned(),
                parameters: Value::Object(tool.input_schema.as_ref().clone()),
            });
            tools.push((name, wanted.clone()));
        }

        let info = service.peer_info();
        let implementation = info.as_ref().and_then(|info| info.server_info.as_ref());
        let record = McpServerRecord {
            server: settings.name.clone(),
            server_name: implementation.map(|server| server.name.clone()),
            server_version: implementation.map(|server| server.version.clone()),
            protocol_version: info
                .as_ref()
                .map(|info| info.protocol_version.to_string())
                .unwrap_or_default(),
            setup_ms: u64::try_from(started.elapsed().as_millis()).expect("setup ms fit u64"),
            tools: specs,
        };
        Ok(Self {
            service,
            tools,
            call_timeout_ms: u64::from(settings.call_timeout_s) * 1000,
            record,
        })
    }

    async fn set_up(
        settings: &McpSettings,
        key: Option<&str>,
    ) -> Result<(RunningService<RoleClient, Client>, Vec<rmcp::model::Tool>), SetupProblem> {
        // Our own client, so redirects are refused as the provider's are.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| SetupProblem::Client(error.to_string()))?;
        let mut config = StreamableHttpClientTransportConfig::with_uri(settings.url.as_str())
            .reinit_on_expired_session(false)
            .control_request_timeout(Duration::from_millis(CANCEL_SEND_TIMEOUT_MS))
            .max_sse_event_size(SSE_EVENT_CAP_BYTES);
        config.retry_config = Arc::new(NeverRetry::default());
        if let (Some(key_settings), Some(key)) = (&settings.key, key) {
            if key_settings.header == "authorization" {
                config = config.auth_header(key);
            } else {
                let name = reqwest::header::HeaderName::from_bytes(key_settings.header.as_bytes())
                    .expect("the profile checked the header name");
                let value =
                    reqwest::header::HeaderValue::from_str(key).map_err(|_| SetupProblem::Key)?;
                config = config.custom_headers([(name, value)].into_iter().collect());
            }
        }
        let transport = StreamableHttpClientTransport::with_client(http, config);
        let service = rmcp::serve_client(Client, transport)
            .await
            .map_err(|error| SetupProblem::Initialize(error.to_string()))?;

        // Sent as plain requests: `rmcp`'s `list_tools` keeps answers in
        // a cache of its own.
        let mut listed = Vec::new();
        let mut cursor = None;
        for _ in 0..TOOL_LIST_CAP_PAGES {
            let request = ClientRequest::ListToolsRequest(ListToolsRequest::with_param(
                PaginatedRequestParams::default().with_cursor(cursor),
            ));
            let page = match service.send_request(request).await {
                Ok(ServerResult::ListToolsResult(page)) => page,
                Ok(_) => {
                    return Err(SetupProblem::ListTools(
                        "the server answered with something else".to_owned(),
                    ));
                }
                Err(error) => return Err(SetupProblem::ListTools(error.to_string())),
            };
            listed.extend(page.tools);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok((service, listed));
            }
        }
        Err(SetupProblem::TooManyPages)
    }

    pub fn specs(&self) -> &[ToolSpec] {
        &self.record.tools
    }

    pub fn record(&self) -> &McpServerRecord {
        &self.record
    }

    pub fn offers(&self, name: &str) -> bool {
        self.tools.iter().any(|(offered, _)| offered == name)
    }

    /// Calls one of the offered tools, by the name the model sees, with
    /// the arguments as the model wrote them.
    pub async fn call(&self, name: &str, arguments: &str, deadline_ms: u64) -> ToolOutcome {
        let (_, tool) = self
            .tools
            .iter()
            .find(|(offered, _)| offered == name)
            .expect("the tool box calls only tools this server offers");
        let arguments = match arguments_object(arguments) {
            Ok(arguments) => arguments,
            Err(problem) => return ToolOutcome::Failed(problem),
        };
        let timeout_ms = self.call_timeout_ms.min(deadline_ms);
        let mut params = CallToolRequestParams::new(tool.clone());
        params.arguments = Some(arguments);
        // A plain request, sent once: `rmcp`'s `call_tool` sends it again
        // when a server asks for input. On timeout `rmcp` sends MCP's
        // cancellation notice.
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let options = PeerRequestOptions::with_timeout(Duration::from_millis(timeout_ms));
        let reply = match self
            .service
            .send_request_with_option(request, options)
            .await
        {
            Ok(handle) => handle.await_response().await,
            Err(error) => Err(error),
        };
        match reply {
            Ok(ServerResult::CallToolResult(result)) => outcome(result),
            Ok(ServerResult::InputRequiredResult(_)) => ToolOutcome::Failed(
                "jakkals: the server asked for input, which Jakkals doesn't give".to_owned(),
            ),
            Ok(ServerResult::CreateTaskResult(_)) => ToolOutcome::Failed(
                "jakkals: the server ran the call as a task, which Jakkals doesn't follow"
                    .to_owned(),
            ),
            Ok(_) => ToolOutcome::Failed(
                "jakkals: the server answered the call with something else".to_owned(),
            ),
            Err(ServiceError::McpError(error)) => {
                ToolOutcome::Failed(format!("MCP error {}: {}", error.code.0, error.message))
            }
            Err(ServiceError::Timeout { .. }) => ToolOutcome::Failed(format!(
                "jakkals: the call ran past its {} ms and was cancelled",
                timeout_ms
            )),
            Err(error) => ToolOutcome::Failed(format!("jakkals: the call failed: {error}")),
        }
    }
}

/// The model's arguments as the JSON object MCP sends; nothing written
/// is taken as no arguments.
fn arguments_object(arguments: &str) -> Result<Map<String, Value>, String> {
    if arguments.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(arguments) {
        Ok(Value::Object(object)) => Ok(object),
        _ => Err("the arguments must be a JSON object".to_owned()),
    }
}

/// A call's result as the model reads it.
fn outcome(result: CallToolResult) -> ToolOutcome {
    let has_text = result
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Text(_)));
    let mut parts = Vec::with_capacity(result.content.len() + 1);
    if !has_text && let Some(structured) = &result.structured_content {
        parts.push(structured.to_string());
    }
    for block in result.content {
        let left_out = match block {
            ContentBlock::Text(text) => {
                parts.push(text.text);
                continue;
            }
            ContentBlock::Image(_) => "image",
            ContentBlock::Audio(_) => "audio",
            ContentBlock::Resource(_) => "resource",
            ContentBlock::ResourceLink(_) => "resource link",
            // A kind added to MCP after this was written.
            _ => "content",
        };
        parts.push(format!("[jakkals: {left_out} left out]"));
    }
    let text = parts.join("\n");
    if result.is_error == Some(true) {
        ToolOutcome::Failed(text)
    } else {
        ToolOutcome::Ok(text)
    }
}

#[cfg(test)]
mod tests;
