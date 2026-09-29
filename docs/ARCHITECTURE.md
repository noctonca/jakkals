# Architecture

Status: **draft**. The fixed decisions below are settled; the profile
fields and event shapes are a first proposal, to be settled before the
code that reads or writes them.

## What Jakkals is

A command-line coding agent for one task at a time: it takes a task, a
model and a profile, works in one directory until it has an answer or
hits a limit, and writes every step to stdout as JSON lines. It is
built to be a fair bench: when two runs differ, the difference is in
the model or in the profile, both recorded, never in something the
harness did on its own.

It is not an interactive assistant (no chat, no TUI), and it does not
keep memory between runs; a memory, if wanted, is an MCP server the
profile names.

## Fixed decisions

| Decision | Why |
|---|---|
| Our own loop, not an agent framework | Frameworks add behaviour of their own (retries, extra turns, loop detection) that no profile records. The loop is small enough to own. |
| Rust, one crate | The maintainer's language; one binary to install; the types carry the wire formats. Split into crates only when one crate hurts. |
| OpenAI-compatible chat completions, own serde types | OpenRouter and local servers (LM Studio) both speak it. Our own types keep the fields generic clients drop: `usage.cost`, the provider, the generation id. |
| MCP through the official Rust SDK (`rmcp`) | Maintained by the protocol's own project; streamable HTTP client. |
| Non-streaming first | A whole reply carries its usage and cost; streaming comes later if time-to-first-token matters. |
| JSON lines on stdout, logs on stderr | Another program runs Jakkals and reads the events; a human reads stderr. |
| MIT OR Apache-2.0 | The Rust ecosystem's default. |

## The shape

```text
jakkals run --profile p.toml --model m --cwd dir --prompt "…"
   │
   ├─ profile ── read, validated, hashed ─────────────┐
   │                                                   ▼
   ├─ provider (HTTP) ◀── loop ──▶ tools ── local: read, list, search, shell
   │                        │           └─ MCP servers named in the profile
   │                        ▼
   └──────────────── events (stdout, JSON lines) ──▶ caller
```

The loop sends the conversation to the provider, runs the tool calls in
the reply, appends the results and repeats, until the model answers
without tool calls or a limit is hit. The provider, tools, clock and
event sink are traits, so tests drive the loop with canned replies.

## The profile (proposed)

One TOML file; every field has a documented default, and the run's
first event carries the profile's hash.

| Field | What it sets |
|---|---|
| `system_prompt` | The system message, verbatim. Empty is allowed. |
| `tools.local` | Which local tools exist: `read`, `list`, `search`, `shell`. |
| `tools.shell_allow` | The shell's allowlist, as command prefixes. |
| `mcp.<name>` | An MCP server: `url`, and the environment variable holding its key; optional tool allow or deny list. |
| `limits.steps` | Most model calls in a run. |
| `limits.cost_usd` | Stop once the reported cost passes this. |
| `limits.tokens` | Stop once total tokens pass this. |
| `limits.wall_s` | The run's deadline. |
| `limits.tool_output_bytes` | A tool result longer than this is cut, and the cut is marked. |
| `provider.base_url` | OpenRouter by default; any compatible server. The key comes from an environment variable the profile names. |
| `provider.params` | Temperature, max tokens and the like, passed through as given. |

## Events (proposed)

One JSON object per line, each with `type` and a run-relative time.

| `type` | Carries |
|---|---|
| `start` | Jakkals version, profile hash, model, tool names. |
| `model_call` | Step, generation id, model and provider that served it, input/output/cached tokens, cost, duration, finish reason. |
| `tool_call` | Step, tool, arguments, result size, whether it was cut or refused, duration. |
| `answer` | The final text. |
| `exit` | Why the run ended: `done`, `limit` (which one), `refused`, `error` (typed), and the totals. |

A run with no `exit` line did not end cleanly and is void.

## Survivable failures (proposed)

Only these end in something other than an error exit, each with an
event: a tool that fails (its error goes back to the model as the
result); a tool call the allowlist refuses (the refusal goes back to
the model); a limit reached (exit `limit`). Whether a provider error is
retried, and how often, is a profile choice defaulting to no.

## The dependency register

Every dependency has a row here before it enters `Cargo.toml`.

| Crate | Why | In `Cargo.toml` |
|---|---|---|
| `clap` (derive) | The CLI's parsing and help. | yes |
| `tokio` | `rmcp` and `reqwest` are async; one runtime for both. | not yet |
| `reqwest` (rustls) | HTTP to the provider. | not yet |
| `serde`, `serde_json` | Wire types, events. | not yet |
| `toml` | The profile. | not yet |
| `rmcp` | MCP client. | not yet |
| `ignore`, `grep-searcher` | The `list` and `search` tools, with ripgrep's ignore rules. | not yet |
| `sha2` | The profile hash. | not yet |

## Open questions

- Context size: what a profile may do when the conversation outgrows a
  budget (nothing, and end the run; or a declared trimming rule).
- The shell tool: prefix allowlist only, or also a sandbox.
- Whether the generation id alone is enough for cost, or the run also
  reads each generation's final cost after the fact.
