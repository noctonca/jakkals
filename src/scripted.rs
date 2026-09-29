//! Scripted edges for tests: a provider that replays canned replies,
//! tools that replay canned outcomes, a clock that moves only when they
//! say so, and a sink that keeps what it gets. No network, no waiting.

use std::cell::Cell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use crate::clock::Clock;
use crate::conversation::{Message, ToolCall, ToolSpec};
use crate::events::{Record, Sink};
use crate::provider::{Provider, ProviderError, Reply, Request, Usage};
use crate::tools::{ToolOutcome, Tools};

/// Drives a future whose edges are all scripted, so it never waits.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("scripted edges never wait"),
    }
}

/// Virtual time, shared by the scripted edges.
#[derive(Clone, Default)]
pub struct VirtualClock {
    now_ms: Rc<Cell<u64>>,
}

impl VirtualClock {
    pub fn advance(&self, ms: u64) {
        self.now_ms.set(self.now_ms.get() + ms);
    }
}

impl Clock for VirtualClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.get()
    }
}

/// What the provider saw on one call.
pub struct SeenRequest {
    pub messages: Vec<Message>,
    pub deadline_ms: u64,
}

pub struct ScriptedProvider {
    clock: VirtualClock,
    /// Each reply, with how long the call takes.
    replies: VecDeque<(Result<Reply, ProviderError>, u64)>,
    pub seen: Vec<SeenRequest>,
}

impl ScriptedProvider {
    pub fn new(clock: &VirtualClock) -> Self {
        Self {
            clock: clock.clone(),
            replies: VecDeque::new(),
            seen: Vec::new(),
        }
    }

    pub fn then(mut self, reply: Result<Reply, ProviderError>, takes_ms: u64) -> Self {
        self.replies.push_back((reply, takes_ms));
        self
    }
}

impl Provider for ScriptedProvider {
    async fn complete(&mut self, request: Request<'_>) -> Result<Reply, ProviderError> {
        self.seen.push(SeenRequest {
            messages: request.messages.to_vec(),
            deadline_ms: request.deadline_ms,
        });
        let (reply, takes_ms) = self
            .replies
            .pop_front()
            .expect("the script has a reply for every call");
        self.clock.advance(takes_ms);
        reply
    }
}

pub struct ScriptedTools {
    clock: VirtualClock,
    specs: Vec<ToolSpec>,
    outcomes: VecDeque<(ToolOutcome, u64)>,
    pub seen: Vec<ToolCall>,
}

impl ScriptedTools {
    pub fn new(clock: &VirtualClock, names: &[&str]) -> Self {
        let specs = names
            .iter()
            .map(|name| ToolSpec {
                name: (*name).to_owned(),
                description: format!("The {name} tool."),
                parameters: serde_json::json!({"type": "object"}),
            })
            .collect();
        Self {
            clock: clock.clone(),
            specs,
            outcomes: VecDeque::new(),
            seen: Vec::new(),
        }
    }

    pub fn then(mut self, outcome: ToolOutcome, takes_ms: u64) -> Self {
        self.outcomes.push_back((outcome, takes_ms));
        self
    }
}

impl Tools for ScriptedTools {
    fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn call(&mut self, call: &ToolCall, _deadline_ms: u64) -> ToolOutcome {
        self.seen.push(call.clone());
        let (outcome, takes_ms) = self
            .outcomes
            .pop_front()
            .expect("the script has an outcome for every call");
        self.clock.advance(takes_ms);
        outcome
    }
}

#[derive(Default)]
pub struct KeptSink {
    pub records: Vec<Record>,
}

impl Sink for KeptSink {
    fn emit(&mut self, record: Record) {
        self.records.push(record);
    }
}

/// A reply with usage and nothing else set.
pub fn reply(prompt_tokens: u32, completion_tokens: u32, cost_nano_usd: Option<u64>) -> Reply {
    Reply {
        text: None,
        tool_calls: Vec::new(),
        usage: Usage {
            prompt_tokens,
            completion_tokens,
            cached_tokens: None,
            reasoning_tokens: None,
            cost_nano_usd,
        },
        generation_id: None,
        model: None,
        provider: None,
        finish_reason: None,
    }
}

pub fn answer(text: &str, usage: Reply) -> Reply {
    Reply {
        text: Some(text.to_owned()),
        finish_reason: Some("stop".to_owned()),
        ..usage
    }
}

pub fn calls(names: &[&str], usage: Reply) -> Reply {
    let tool_calls = names
        .iter()
        .enumerate()
        .map(|(index, name)| ToolCall {
            id: format!("call-{index}"),
            name: (*name).to_owned(),
            arguments: "{}".to_owned(),
        })
        .collect();
    Reply {
        tool_calls,
        finish_reason: Some("tool_calls".to_owned()),
        ..usage
    }
}
