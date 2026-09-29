//! The conversation as the loop sees it: our own types, turned into a
//! provider's wire format only at the provider's edge.

use serde::Serialize;

/// One message of the conversation, in the order it was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    System {
        text: String,
    },
    User {
        text: String,
    },
    Assistant {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    /// A tool's result, answering the assistant's call with `call_id`.
    Tool {
        call_id: String,
        text: String,
    },
}

/// A tool call as the model wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    /// The provider's id for the call, echoed back with the result.
    pub id: String,
    pub name: String,
    /// The arguments exactly as the model wrote them: usually JSON, but
    /// nothing checks that before the tool does.
    pub arguments: String,
}

/// A tool as offered to the model.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    /// Part of the prompt: keep it short.
    pub description: String,
    /// The arguments' JSON Schema.
    pub parameters: serde_json::Value,
}
