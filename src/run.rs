//! The loop: send the conversation, run the tool calls in the reply,
//! append the results, repeat, until the model answers without tool
//! calls or a limit is hit. Every step is an event; nothing else happens.

use serde::Serialize;

use crate::clock::Clock;
use crate::conversation::{Message, ToolCall};
use crate::events::{Event, LimitKind, Outcome, Record, RunError, Sink, Totals};
use crate::provider::{Provider, ProviderError, Reply, Request};
use crate::tools::{ToolOutcome, ToolStatus, Tools};

/// What one run is asked to do.
pub struct Task<'a> {
    /// Sent verbatim as the system message; empty means none is sent.
    pub system_prompt: &'a str,
    pub prompt: &'a str,
    pub model: &'a str,
    pub profile_hash: &'a str,
}

/// The profile's limits, in the profile's units except cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Limits {
    /// Most model calls in a run. Required by the profile.
    pub steps: u32,
    /// The run's deadline, in seconds. Required by the profile.
    pub wall_s: u32,
    pub cost_nano_usd: Option<u64>,
    /// Input plus output tokens, summed over the run's calls.
    pub tokens: Option<u64>,
    /// Most prompt tokens one call may report.
    pub context_tokens: Option<u32>,
    /// A tool result longer than this is cut, at a character boundary.
    pub tool_output_bytes: u32,
}

impl Limits {
    fn wall_ms(&self) -> u64 {
        u64::from(self.wall_s) * 1000
    }
}

/// Runs one task to its end and returns why it ended. The last event
/// emitted is always the `exit` carrying the same outcome.
pub async fn run<P, T, C, S>(
    task: &Task<'_>,
    limits: &Limits,
    provider: &mut P,
    tools: &mut T,
    clock: &C,
    sink: &mut S,
) -> Outcome
where
    P: Provider,
    T: Tools,
    C: Clock,
    S: Sink,
{
    // The profile refuses these; reaching here without them is a bug.
    assert!(limits.steps > 0, "limits.steps is required and positive");
    assert!(limits.wall_s > 0, "limits.wall_s is required and positive");

    let mut run = Loop {
        clock,
        sink,
        totals: Totals {
            cost_nano_usd: Some(0),
            ..Totals::default()
        },
    };
    run.emit(Event::Start {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        profile_hash: task.profile_hash.to_owned(),
        model: task.model.to_owned(),
        tools: tools.specs().iter().map(|spec| spec.name.clone()).collect(),
        limits: *limits,
    });

    let mut messages = Vec::new();
    if !task.system_prompt.is_empty() {
        messages.push(Message::System {
            text: task.system_prompt.to_owned(),
        });
    }
    messages.push(Message::User {
        text: task.prompt.to_owned(),
    });

    let outcome = loop {
        // Tools take time too, so the deadline is checked before every
        // call, not only after replies.
        let Some(deadline_ms) = limits
            .wall_ms()
            .checked_sub(run.clock.now_ms())
            .filter(|ms| *ms > 0)
        else {
            break Outcome::Limit {
                which: LimitKind::WallS,
            };
        };

        let step = run.totals.steps + 1;
        run.totals.steps = step;
        run.emit(Event::ModelRequest {
            step,
            messages: u32::try_from(messages.len()).expect("fewer than 2^32 messages"),
        });
        let started_ms = run.clock.now_ms();
        let request = Request {
            model: task.model,
            messages: &messages,
            tools: tools.specs(),
            deadline_ms,
        };
        let reply = match provider.complete(request).await {
            Ok(reply) => reply,
            Err(ProviderError::Deadline) => {
                break Outcome::Limit {
                    which: LimitKind::WallS,
                };
            }
            Err(error) => {
                break Outcome::Error {
                    error: RunError::Provider(error),
                };
            }
        };
        let duration_ms = run.clock.now_ms() - started_ms;
        run.record_call(step, &reply, duration_ms);

        if limits.cost_nano_usd.is_some() && reply.usage.cost_nano_usd.is_none() {
            break Outcome::Error {
                error: RunError::CostUnreported,
            };
        }

        let Reply {
            text,
            tool_calls,
            usage,
            ..
        } = reply;
        if tool_calls.is_empty() {
            // An answer ends the run even past a limit: the model is done,
            // and the limits bound work still to come.
            run.emit(Event::Answer {
                text: text.unwrap_or_default(),
            });
            break Outcome::Done;
        }

        // Checked before running the tools, whose results no model call
        // would read.
        if let Some(which) = run.limit_passed(limits, step, usage.prompt_tokens) {
            break Outcome::Limit { which };
        }

        messages.push(Message::Assistant {
            text,
            tool_calls: tool_calls.clone(),
        });
        for call in &tool_calls {
            let deadline_ms = limits.wall_ms().saturating_sub(run.clock.now_ms());
            if deadline_ms == 0 {
                // The next pass through the loop ends the run on the
                // deadline; the calls left unrun have no events.
                break;
            }
            let result = run
                .call_tool(tools, call, step, deadline_ms, limits.tool_output_bytes)
                .await;
            messages.push(Message::Tool {
                call_id: call.id.clone(),
                text: result,
            });
        }
    };

    run.emit(Event::Exit {
        outcome: outcome.clone(),
        totals: run.totals,
    });
    outcome
}

/// The loop's state between steps.
struct Loop<'a, C, S> {
    clock: &'a C,
    sink: &'a mut S,
    totals: Totals,
}

impl<C: Clock, S: Sink> Loop<'_, C, S> {
    fn emit(&mut self, event: Event) {
        let t_ms = self.clock.now_ms();
        self.sink.emit(Record { event, t_ms });
    }

    fn record_call(&mut self, step: u32, reply: &Reply, duration_ms: u64) {
        let usage = reply.usage;
        self.totals.input_tokens += u64::from(usage.prompt_tokens);
        self.totals.output_tokens += u64::from(usage.completion_tokens);
        self.totals.cost_nano_usd = match (self.totals.cost_nano_usd, usage.cost_nano_usd) {
            // u64 nano-dollars hold 18 billion dollars: overflow is a bug.
            (Some(sum), Some(cost)) => {
                Some(sum.checked_add(cost).expect("run cost fits u64 nano-USD"))
            }
            _ => None,
        };
        self.emit(Event::ModelCall {
            step,
            generation_id: reply.generation_id.clone(),
            model: reply.model.clone(),
            provider: reply.provider.clone(),
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            cached_tokens: usage.cached_tokens,
            cost_nano_usd: usage.cost_nano_usd,
            duration_ms,
            finish_reason: reply.finish_reason.clone(),
        });
    }

    /// The first limit the run has passed after `step`'s reply, in the
    /// order the profile lists them.
    fn limit_passed(&self, limits: &Limits, step: u32, prompt_tokens: u32) -> Option<LimitKind> {
        if step >= limits.steps {
            return Some(LimitKind::Steps);
        }
        if let (Some(limit), Some(cost)) = (limits.cost_nano_usd, self.totals.cost_nano_usd)
            && cost > limit
        {
            return Some(LimitKind::CostUsd);
        }
        if let Some(limit) = limits.tokens
            && self.totals.input_tokens + self.totals.output_tokens > limit
        {
            return Some(LimitKind::Tokens);
        }
        if let Some(limit) = limits.context_tokens
            && prompt_tokens > limit
        {
            return Some(LimitKind::ContextTokens);
        }
        None
    }

    /// Runs one tool call and returns the text the model will read.
    async fn call_tool<T: Tools>(
        &mut self,
        tools: &mut T,
        call: &ToolCall,
        step: u32,
        deadline_ms: u64,
        cap_bytes: u32,
    ) -> String {
        let started_ms = self.clock.now_ms();
        let outcome = if tools.specs().iter().any(|spec| spec.name == call.name) {
            tools.call(call, deadline_ms).await
        } else {
            ToolOutcome::Refused(format!("jakkals: there is no tool named `{}`", call.name))
        };
        let duration_ms = self.clock.now_ms() - started_ms;
        let status: ToolStatus = outcome.status();
        let text = outcome.into_text();
        let result_bytes = u64::try_from(text.len()).expect("result shorter than 2^64 bytes");
        let (text, cut) = cut(text, cap_bytes);
        self.totals.tool_calls += 1;
        self.emit(Event::ToolCall {
            step,
            tool: call.name.clone(),
            arguments: call.arguments.clone(),
            status,
            result_bytes,
            cut,
            duration_ms,
        });
        text
    }
}

/// Cuts `text` to at most `cap_bytes` at a character boundary and marks
/// the cut, so the model knows the result is partial.
fn cut(text: String, cap_bytes: u32) -> (String, bool) {
    let cap = usize::try_from(cap_bytes).expect("u32 fits usize");
    if text.len() <= cap {
        return (text, false);
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let marked = format!(
        "{}\n[jakkals: output cut at {end} of {} bytes]",
        &text[..end],
        text.len()
    );
    (marked, true)
}

#[cfg(test)]
mod tests;
