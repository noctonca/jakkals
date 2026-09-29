//! The model provider, as the loop sees it. The HTTP client for
//! OpenAI-compatible servers lives at the edge and fills these types.

use serde::Serialize;

use crate::conversation::{Message, ToolCall, ToolSpec};

// The loop is generic over its edges and never boxes them or sends them
// across threads, so the futures need no `Send` bound.
#[allow(async_fn_in_trait)]
pub trait Provider {
    /// One model call. It must return by `request.deadline_ms` from now,
    /// with [`ProviderError::Deadline`] if the reply isn't in by then.
    async fn complete(&mut self, request: Request<'_>) -> Result<Reply, ProviderError>;
}

pub struct Request<'a> {
    pub model: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    /// Milliseconds left before the run's deadline.
    pub deadline_ms: u64,
}

/// A whole (non-streamed) reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub generation_id: Option<String>,
    /// The model that served the call, which may differ from the one
    /// asked for (a router).
    pub model: Option<String>,
    /// The upstream provider that served the call, where reported.
    pub provider: Option<String>,
    pub finish_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cached_tokens: Option<u32>,
    /// The cost the reply reports, in billionths of a US dollar; `None`
    /// when the server reports none (a local server), which is not 0.
    pub cost_nano_usd: Option<u64>,
}

/// Why a model call failed. Classified from structured fields only,
/// never by matching a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
// Tagged `provider_error` because it sits inside a run error tagged
// `kind`: {"kind":"provider","provider_error":"status",...}.
#[serde(tag = "provider_error", rename_all = "snake_case")]
pub enum ProviderError {
    /// The server answered with an error status.
    Status { status: u16, body: String },
    /// The request didn't reach the server or the reply didn't arrive.
    Transport { detail: String },
    /// The reply arrived but isn't what the API promises.
    Malformed { detail: String },
    /// The run's deadline passed during the call.
    Deadline,
}
