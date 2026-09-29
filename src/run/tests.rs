//! Scripted runs: canned replies in, exact events and exit out.

use super::*;
use crate::events::JsonLines;
use crate::scripted::{
    KeptSink, ScriptedProvider, ScriptedTools, VirtualClock, answer, block_on, calls, reply,
};
use crate::tools::mcp::McpServerRecord;
use crate::transcript::{JsonLinesTranscript, NoTranscript};

const TASK: Task<'static> = Task {
    system_prompt: "You are a test.",
    prompt: "What is in the box?",
    model: "test/model",
    profile_hash: "hash",
    transcript: None,
};

const LIMITS: Limits = Limits {
    steps: 10,
    wall_s: 60,
    cost_nano_usd: None,
    tokens: None,
    context_tokens: None,
    tool_output_bytes: 1000,
};

struct Ran {
    outcome: Outcome,
    events: Vec<Event>,
    /// The transcript's lines, reasoning included.
    transcript: Vec<serde_json::Value>,
    provider: ScriptedProvider,
    tools: ScriptedTools,
}

fn run_scripted(
    task: &Task<'_>,
    limits: &Limits,
    clock: &VirtualClock,
    mut provider: ScriptedProvider,
    mut tools: ScriptedTools,
) -> Ran {
    let mut sink = KeptSink::default();
    let mut written = Vec::new();
    let outcome = block_on(run(
        task,
        limits,
        &mut provider,
        &mut tools,
        clock,
        &mut sink,
        &mut JsonLinesTranscript::new(&mut written, true),
    ));
    let transcript = String::from_utf8(written)
        .expect("the transcript is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect();
    let events: Vec<Event> = sink
        .records
        .into_iter()
        .map(|record| record.event)
        .collect();
    assert!(
        matches!(events.last(), Some(Event::Exit { outcome: exit, .. }) if *exit == outcome),
        "the last event is the exit, carrying the outcome"
    );
    Ran {
        outcome,
        events,
        transcript,
        provider,
        tools,
    }
}

fn exit_totals(ran: &Ran) -> Totals {
    match ran.events.last() {
        Some(Event::Exit { totals, .. }) => *totals,
        _ => unreachable!("run_scripted checked the exit"),
    }
}

fn kinds(events: &[Event]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            Event::Start { .. } => "start",
            Event::McpServer(_) => "mcp_server",
            Event::ModelRequest { .. } => "model_request",
            Event::ModelCall { .. } => "model_call",
            Event::ToolCall { .. } => "tool_call",
            Event::Answer { .. } => "answer",
            Event::Exit { .. } => "exit",
        })
        .collect()
}

#[test]
fn the_event_stream_has_the_documented_shape() {
    let clock = VirtualClock::default();
    let mut first = calls(&["read"], reply(100, 10, Some(1500)));
    first.generation_id = Some("gen-1".to_owned());
    first.model = Some("test/model-served".to_owned());
    first.provider = Some("TestCloud".to_owned());
    let mut provider = ScriptedProvider::new(&clock)
        .then(Ok(first), 250)
        .then(Ok(answer("A cat.", reply(130, 5, Some(2000)))), 300);
    let mut tools =
        ScriptedTools::new(&clock, &["read"]).then(ToolOutcome::Ok("a cat".to_owned()), 20);

    let mut out = Vec::new();
    let mut sink = JsonLines::new(&mut out);
    block_on(run(
        &TASK,
        &LIMITS,
        &mut provider,
        &mut tools,
        &clock,
        &mut sink,
        &mut NoTranscript,
    ));

    let version = env!("CARGO_PKG_VERSION");
    let expected = [
        format!(
            r#"{{"type":"start","version":"{version}","profile_hash":"hash","model":"test/model","tools":["read"],"sandbox":null,"limits":{{"steps":10,"wall_s":60,"cost_nano_usd":null,"tokens":null,"context_tokens":null,"tool_output_bytes":1000}},"transcript":null,"t_ms":0}}"#
        ),
        r#"{"type":"model_request","step":1,"messages":2,"t_ms":0}"#.to_owned(),
        r#"{"type":"model_call","step":1,"generation_id":"gen-1","model":"test/model-served","provider":"TestCloud","input_tokens":100,"output_tokens":10,"cached_tokens":null,"reasoning_tokens":null,"cost_nano_usd":1500,"duration_ms":250,"finish_reason":"tool_calls","t_ms":250}"#.to_owned(),
        r#"{"type":"tool_call","step":1,"tool":"read","arguments":"{}","status":"ok","result_bytes":5,"cut":false,"duration_ms":20,"t_ms":270}"#.to_owned(),
        r#"{"type":"model_request","step":2,"messages":4,"t_ms":270}"#.to_owned(),
        r#"{"type":"model_call","step":2,"generation_id":null,"model":null,"provider":null,"input_tokens":130,"output_tokens":5,"cached_tokens":null,"reasoning_tokens":null,"cost_nano_usd":2000,"duration_ms":300,"finish_reason":"stop","t_ms":570}"#.to_owned(),
        r#"{"type":"answer","text":"A cat.","t_ms":570}"#.to_owned(),
        r#"{"type":"exit","reason":"done","totals":{"steps":2,"tool_calls":1,"input_tokens":230,"output_tokens":15,"cost_nano_usd":3500},"t_ms":570}"#.to_owned(),
    ];
    let written = String::from_utf8(out).expect("events are UTF-8");
    let lines: Vec<&str> = written.lines().collect();
    assert_eq!(lines, expected);
    assert!(written.ends_with('\n'), "every event ends its line");
}

#[test]
fn the_conversation_carries_system_prompt_calls_and_results() {
    let clock = VirtualClock::default();
    let provider = ScriptedProvider::new(&clock)
        .then(Ok(calls(&["read"], reply(10, 1, None))), 1)
        .then(Ok(answer("done", reply(20, 1, None))), 1);
    let tools =
        ScriptedTools::new(&clock, &["read"]).then(ToolOutcome::Ok("contents".to_owned()), 1);
    let ran = run_scripted(&TASK, &LIMITS, &clock, provider, tools);

    assert_eq!(ran.outcome, Outcome::Done);
    let second = &ran.provider.seen[1].messages;
    assert_eq!(
        *second,
        vec![
            Message::System {
                text: "You are a test.".to_owned()
            },
            Message::User {
                text: "What is in the box?".to_owned()
            },
            Message::Assistant {
                text: None,
                tool_calls: ran.tools.seen.clone(),
            },
            Message::Tool {
                call_id: "call-0".to_owned(),
                text: "contents".to_owned()
            },
        ]
    );
}

#[test]
fn an_empty_system_prompt_sends_no_system_message() {
    let clock = VirtualClock::default();
    let task = Task {
        system_prompt: "",
        ..TASK
    };
    let provider = ScriptedProvider::new(&clock).then(Ok(answer("hi", reply(1, 1, None))), 1);
    let ran = run_scripted(
        &task,
        &LIMITS,
        &clock,
        provider,
        ScriptedTools::new(&clock, &[]),
    );
    assert_eq!(
        ran.provider.seen[0].messages,
        vec![Message::User {
            text: "What is in the box?".to_owned()
        }]
    );
}

#[test]
fn an_answer_without_text_is_recorded_as_empty() {
    let clock = VirtualClock::default();
    let provider = ScriptedProvider::new(&clock).then(Ok(reply(1, 0, None)), 1);
    let ran = run_scripted(
        &TASK,
        &LIMITS,
        &clock,
        provider,
        ScriptedTools::new(&clock, &[]),
    );
    assert_eq!(ran.outcome, Outcome::Done);
    assert!(ran.events.contains(&Event::Answer {
        text: String::new()
    }));
}

#[test]
fn a_call_to_a_tool_not_offered_is_refused_and_the_run_goes_on() {
    let clock = VirtualClock::default();
    let provider = ScriptedProvider::new(&clock)
        .then(Ok(calls(&["delete"], reply(10, 1, None))), 1)
        .then(Ok(answer("ok", reply(20, 1, None))), 1);
    let ran = run_scripted(
        &TASK,
        &LIMITS,
        &clock,
        provider,
        ScriptedTools::new(&clock, &["read"]),
    );

    assert_eq!(ran.outcome, Outcome::Done);
    assert!(ran.tools.seen.is_empty(), "the tool set never saw the call");
    assert!(ran.events.iter().any(|event| matches!(
        event,
        Event::ToolCall { tool, status: ToolStatus::Refused, .. } if tool == "delete"
    )));
    assert_eq!(
        ran.provider.seen[1].messages.last(),
        Some(&Message::Tool {
            call_id: "call-0".to_owned(),
            text: "jakkals: there is no tool named `delete`".to_owned()
        })
    );
}

#[test]
fn failed_and_refused_tool_outcomes_go_back_to_the_model() {
    let clock = VirtualClock::default();
    let provider = ScriptedProvider::new(&clock)
        .then(Ok(calls(&["read", "shell"], reply(10, 1, None))), 1)
        .then(Ok(answer("ok", reply(20, 1, None))), 1);
    let tools = ScriptedTools::new(&clock, &["read", "shell"])
        .then(ToolOutcome::Failed("no such file".to_owned()), 1)
        .then(ToolOutcome::Refused("not on the allowlist".to_owned()), 1);
    let ran = run_scripted(&TASK, &LIMITS, &clock, provider, tools);

    let statuses: Vec<ToolStatus> = ran
        .events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCall { status, .. } => Some(*status),
            _ => None,
        })
        .collect();
    assert_eq!(statuses, [ToolStatus::Failed, ToolStatus::Refused]);
    let texts: Vec<&str> = ran.provider.seen[1]
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::Tool { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["no such file", "not on the allowlist"]);
}

#[test]
fn a_long_tool_result_is_cut_at_a_character_boundary_and_marked() {
    let clock = VirtualClock::default();
    let limits = Limits {
        tool_output_bytes: 5,
        ..LIMITS
    };
    // "ééé" is six bytes; five would split the third character.
    let provider = ScriptedProvider::new(&clock)
        .then(Ok(calls(&["read"], reply(10, 1, None))), 1)
        .then(Ok(answer("ok", reply(20, 1, None))), 1);
    let tools = ScriptedTools::new(&clock, &["read"]).then(ToolOutcome::Ok("ééé".to_owned()), 1);
    let ran = run_scripted(&TASK, &limits, &clock, provider, tools);

    assert!(ran.events.iter().any(|event| matches!(
        event,
        Event::ToolCall {
            result_bytes: 6,
            cut: true,
            ..
        }
    )));
    assert_eq!(
        ran.provider.seen[1].messages.last(),
        Some(&Message::Tool {
            call_id: "call-0".to_owned(),
            text: "éé\n[jakkals: output cut at 4 of 6 bytes]".to_owned()
        })
    );
}

#[test]
fn the_step_limit_ends_the_run_without_running_the_last_steps_tools() {
    let clock = VirtualClock::default();
    let limits = Limits { steps: 2, ..LIMITS };
    let provider = ScriptedProvider::new(&clock)
        .then(Ok(calls(&["read"], reply(10, 1, None))), 1)
        .then(Ok(calls(&["read"], reply(20, 1, None))), 1);
    let tools = ScriptedTools::new(&clock, &["read"]).then(ToolOutcome::Ok("x".to_owned()), 1);
    let ran = run_scripted(&TASK, &limits, &clock, provider, tools);

    assert_eq!(
        ran.outcome,
        Outcome::Limit {
            which: LimitKind::Steps
        }
    );
    assert_eq!(ran.tools.seen.len(), 1, "only the first step's tool ran");
    assert_eq!(exit_totals(&ran).steps, 2);
    assert_eq!(
        kinds(&ran.events),
        [
            "start",
            "model_request",
            "model_call",
            "tool_call",
            "model_request",
            "model_call",
            "exit"
        ]
    );
}

#[test]
fn an_answer_on_the_last_step_is_done_not_a_limit() {
    let clock = VirtualClock::default();
    let limits = Limits {
        steps: 1,
        context_tokens: Some(10),
        ..LIMITS
    };
    let provider = ScriptedProvider::new(&clock).then(Ok(answer("done", reply(50, 1, None))), 1);
    let ran = run_scripted(
        &TASK,
        &limits,
        &clock,
        provider,
        ScriptedTools::new(&clock, &[]),
    );
    assert_eq!(ran.outcome, Outcome::Done);
}

#[test]
fn the_deadline_is_passed_to_the_provider_and_ends_the_run() {
    let clock = VirtualClock::default();
    let limits = Limits {
        wall_s: 1,
        ..LIMITS
    };
    let provider =
        ScriptedProvider::new(&clock).then(Ok(calls(&["read"], reply(10, 1, None))), 400);
    // The tool takes the rest of the second, so no second call is made.
    let tools = ScriptedTools::new(&clock, &["read"]).then(ToolOutcome::Ok("x".to_owned()), 600);
    let ran = run_scripted(&TASK, &limits, &clock, provider, tools);

    assert_eq!(ran.provider.seen[0].deadline_ms, 1000);
    assert_eq!(
        ran.outcome,
        Outcome::Limit {
            which: LimitKind::WallS
        }
    );
    assert_eq!(exit_totals(&ran).steps, 1);
}

#[test]
fn tool_calls_left_when_the_deadline_passes_are_not_run() {
    let clock = VirtualClock::default();
    let limits = Limits {
        wall_s: 1,
        ..LIMITS
    };
    let provider =
        ScriptedProvider::new(&clock).then(Ok(calls(&["read", "read"], reply(10, 1, None))), 1);
    let tools = ScriptedTools::new(&clock, &["read"]).then(ToolOutcome::Ok("x".to_owned()), 999);
    let ran = run_scripted(&TASK, &limits, &clock, provider, tools);

    assert_eq!(ran.tools.seen.len(), 1);
    assert_eq!(exit_totals(&ran).tool_calls, 1);
    assert_eq!(
        ran.outcome,
        Outcome::Limit {
            which: LimitKind::WallS
        }
    );
}

#[test]
fn a_provider_deadline_is_the_wall_limit() {
    let clock = VirtualClock::default();
    let provider = ScriptedProvider::new(&clock).then(Err(ProviderError::Deadline), 60_000);
    let ran = run_scripted(
        &TASK,
        &LIMITS,
        &clock,
        provider,
        ScriptedTools::new(&clock, &[]),
    );
    assert_eq!(
        ran.outcome,
        Outcome::Limit {
            which: LimitKind::WallS
        }
    );
    assert_eq!(kinds(&ran.events), ["start", "model_request", "exit"]);
}

#[test]
fn a_provider_error_ends_the_run_with_it() {
    let clock = VirtualClock::default();
    let error = ProviderError::Status {
        status: 429,
        body: "slow down".to_owned(),
    };
    let provider = ScriptedProvider::new(&clock).then(Err(error.clone()), 1);
    let ran = run_scripted(
        &TASK,
        &LIMITS,
        &clock,
        provider,
        ScriptedTools::new(&clock, &[]),
    );
    assert_eq!(
        ran.outcome,
        Outcome::Error {
            error: RunError::Provider(error)
        }
    );
    let exit = serde_json::to_string(ran.events.last().expect("an exit")).expect("serializes");
    assert!(
        exit.starts_with(r#"{"type":"exit","reason":"error","error":{"kind":"provider","provider_error":"status","status":429,"body":"slow down"},"#),
        "{exit}"
    );
}

#[test]
fn the_cost_limit_ends_the_run_once_the_sum_passes_it() {
    let clock = VirtualClock::default();
    let limits = Limits {
        cost_nano_usd: Some(1000),
        ..LIMITS
    };
    let provider = ScriptedProvider::new(&clock)
        .then(Ok(calls(&["read"], reply(10, 1, Some(600)))), 1)
        .then(Ok(calls(&["read"], reply(10, 1, Some(600)))), 1);
    let tools = ScriptedTools::new(&clock, &["read"]).then(ToolOutcome::Ok("x".to_owned()), 1);
    let ran = run_scripted(&TASK, &limits, &clock, provider, tools);

    assert_eq!(
        ran.outcome,
        Outcome::Limit {
            which: LimitKind::CostUsd
        }
    );
    assert_eq!(exit_totals(&ran).cost_nano_usd, Some(1200));
}

#[test]
fn a_cost_limit_that_cannot_be_enforced_ends_the_run() {
    let clock = VirtualClock::default();
    let limits = Limits {
        cost_nano_usd: Some(1000),
        ..LIMITS
    };
    let provider = ScriptedProvider::new(&clock).then(Ok(calls(&["read"], reply(10, 1, None))), 1);
    let ran = run_scripted(
        &TASK,
        &limits,
        &clock,
        provider,
        ScriptedTools::new(&clock, &["read"]),
    );

    assert_eq!(
        ran.outcome,
        Outcome::Error {
            error: RunError::CostUnreported
        }
    );
    assert!(ran.tools.seen.is_empty());
    assert_eq!(
        kinds(&ran.events),
        ["start", "model_request", "model_call", "exit"]
    );
}

#[test]
fn without_a_cost_limit_an_unreported_cost_makes_the_total_null() {
    let clock = VirtualClock::default();
    let provider = ScriptedProvider::new(&clock)
        .then(Ok(calls(&["read"], reply(10, 1, Some(500)))), 1)
        .then(Ok(answer("ok", reply(10, 1, None))), 1);
    let tools = ScriptedTools::new(&clock, &["read"]).then(ToolOutcome::Ok("x".to_owned()), 1);
    let ran = run_scripted(&TASK, &LIMITS, &clock, provider, tools);

    assert_eq!(ran.outcome, Outcome::Done);
    assert_eq!(exit_totals(&ran).cost_nano_usd, None);
}

#[test]
fn the_token_limit_counts_input_and_output_over_the_run() {
    let clock = VirtualClock::default();
    let limits = Limits {
        tokens: Some(100),
        ..LIMITS
    };
    let provider = ScriptedProvider::new(&clock)
        .then(Ok(calls(&["read"], reply(40, 5, None))), 1)
        .then(Ok(calls(&["read"], reply(50, 6, None))), 1);
    let tools = ScriptedTools::new(&clock, &["read"]).then(ToolOutcome::Ok("x".to_owned()), 1);
    let ran = run_scripted(&TASK, &limits, &clock, provider, tools);

    assert_eq!(
        ran.outcome,
        Outcome::Limit {
            which: LimitKind::Tokens
        }
    );
    let totals = exit_totals(&ran);
    assert_eq!((totals.input_tokens, totals.output_tokens), (90, 11));
}

#[test]
fn the_context_limit_reads_the_reported_prompt_tokens() {
    let clock = VirtualClock::default();
    let limits = Limits {
        context_tokens: Some(100),
        ..LIMITS
    };
    // The first prompt is at the budget, not over it; the second passes it.
    let provider = ScriptedProvider::new(&clock)
        .then(Ok(calls(&["read"], reply(100, 1, None))), 1)
        .then(Ok(calls(&["read"], reply(101, 1, None))), 1);
    let tools = ScriptedTools::new(&clock, &["read"]).then(ToolOutcome::Ok("x".to_owned()), 1);
    let ran = run_scripted(&TASK, &limits, &clock, provider, tools);

    assert_eq!(
        ran.outcome,
        Outcome::Limit {
            which: LimitKind::ContextTokens
        }
    );
    assert_eq!(exit_totals(&ran).steps, 2);
}

#[test]
fn cut_leaves_short_text_alone() {
    assert_eq!(cut("abc".to_owned(), 3), ("abc".to_owned(), false));
    assert_eq!(cut(String::new(), 0), (String::new(), false));
}

#[test]
fn each_mcp_server_is_recorded_after_start_in_the_order_offered() {
    let clock = VirtualClock::default();
    let provider =
        ScriptedProvider::new(&clock).then(Ok(answer("Done.", reply(10, 5, Some(1)))), 10);
    let mut tools = ScriptedTools::new(&clock, &["alpha_look", "beta_find"]);
    tools.servers = ["alpha", "beta"]
        .iter()
        .map(|server| McpServerRecord {
            server: (*server).to_owned(),
            server_name: None,
            server_version: None,
            protocol_version: "2025-06-18".to_owned(),
            setup_ms: 5,
            tools: Vec::new(),
        })
        .collect();

    let ran = run_scripted(&TASK, &LIMITS, &clock, provider, tools);
    assert_eq!(
        kinds(&ran.events),
        [
            "start",
            "mcp_server",
            "mcp_server",
            "model_request",
            "model_call",
            "answer",
            "exit"
        ]
    );
    let servers: Vec<&str> = ran
        .events
        .iter()
        .filter_map(|event| match event {
            Event::McpServer(record) => Some(record.server.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(servers, ["alpha", "beta"]);
}

/// The transcript lines' roles, in order.
fn roles(ran: &Ran) -> Vec<&str> {
    ran.transcript
        .iter()
        .map(|line| line["role"].as_str().expect("every line has a role"))
        .collect()
}

#[test]
fn the_transcript_is_the_conversation_with_steps_and_times() {
    let clock = VirtualClock::default();
    let mut first = calls(&["read"], reply(100, 10, Some(1)));
    first.reasoning = Some("Look in the box.".to_owned());
    let provider = ScriptedProvider::new(&clock)
        .then(Ok(first), 250)
        .then(Ok(answer("A cat.", reply(130, 5, Some(1)))), 300);
    let tools = ScriptedTools::new(&clock, &["read"]).then(ToolOutcome::Ok("a cat".to_owned()), 20);
    let ran = run_scripted(&TASK, &LIMITS, &clock, provider, tools);

    let expected = serde_json::json!([
        {"role": "system", "text": "You are a test.", "step": 0, "t_ms": 0},
        {"role": "user", "text": "What is in the box?", "step": 0, "t_ms": 0},
        {"role": "assistant", "text": null, "reasoning": "Look in the box.", "step": 1, "t_ms": 250,
         "tool_calls": [{"id": "call-0", "name": "read", "arguments": "{}"}]},
        {"role": "tool", "call_id": "call-0", "tool": "read", "text": "a cat", "step": 1, "t_ms": 270},
        {"role": "assistant", "text": "A cat.", "reasoning": null, "tool_calls": [], "step": 2, "t_ms": 570},
    ]);
    assert_eq!(serde_json::Value::Array(ran.transcript.clone()), expected);

    // Each request's message count is the lines written before it, so
    // the transcript joins the events.
    let sent: Vec<usize> = ran
        .provider
        .seen
        .iter()
        .map(|seen| seen.messages.len())
        .collect();
    assert_eq!(sent, [2, 4]);
}

#[test]
fn a_reply_a_limit_stops_is_written_but_its_tools_are_not() {
    let clock = VirtualClock::default();
    let limits = Limits { steps: 1, ..LIMITS };
    let provider = ScriptedProvider::new(&clock).then(Ok(calls(&["read"], reply(10, 1, None))), 1);
    let ran = run_scripted(
        &TASK,
        &limits,
        &clock,
        provider,
        ScriptedTools::new(&clock, &["read"]),
    );

    assert_eq!(
        ran.outcome,
        Outcome::Limit {
            which: LimitKind::Steps
        }
    );
    assert_eq!(roles(&ran), ["system", "user", "assistant"]);
}

#[test]
fn a_refused_call_and_a_cut_result_are_written_as_the_model_got_them() {
    let clock = VirtualClock::default();
    let limits = Limits {
        tool_output_bytes: 60,
        ..LIMITS
    };
    let provider = ScriptedProvider::new(&clock)
        .then(Ok(calls(&["read", "write"], reply(10, 1, None))), 1)
        .then(Ok(answer("Done.", reply(10, 1, None))), 1);
    let tools = ScriptedTools::new(&clock, &["read"]).then(ToolOutcome::Ok("x".repeat(70)), 1);
    let ran = run_scripted(&TASK, &limits, &clock, provider, tools);

    let results: Vec<&str> = ran
        .transcript
        .iter()
        .filter(|line| line["role"] == "tool")
        .map(|line| line["text"].as_str().expect("a tool line has text"))
        .collect();
    let sent: Vec<String> = ran.provider.seen[1]
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::Tool { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results, sent);
    assert!(results[0].ends_with("\n[jakkals: output cut at 60 of 70 bytes]"));
    assert!(results[1].contains("no tool named `write`"));
}
