# Architecture

Status: **draft**. The fixed decisions below are settled, and so are
the profile fields Jakkals reads today. The fields of capabilities not
built yet (tools, MCP) and the event shapes are a first proposal, to be
settled by the code that reads or writes them.

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

The loop's rules, each pinned by a scripted test in `src/run/tests.rs`:

- An answer (a reply without tool calls) ends the run as `done`, even
  past a limit: the limits bound work still to come.
- After a reply with tool calls, the limits are checked before the
  tools run, in the profile's order (`steps`, `cost_usd`, `tokens`,
  `context_tokens`), so no tool runs whose result no model call would
  read.
- The deadline (`wall_s`) is checked before every model call, and the
  time left is handed to the provider and each tool as their own
  deadline. A provider that runs out of it ends the run as `limit`
  `wall_s`; tool calls still queued when it passes are not run.
- A call to a tool the profile doesn't offer never reaches the tools:
  the loop refuses it, and the refusal goes back to the model.
- Tool calls in one reply run in the order the model wrote them.

## The profile

One TOML file, read by [`src/profile.rs`](../src/profile.rs). The run's
first event carries its hash: `sha256:` and the hex digest of the
file's bytes, so `shasum -a 256` matches a run to its file. What the
defaults mean is fixed by the Jakkals version, which the same event
carries.

`limits.steps` and `limits.wall_s` are required, so every run is
bounded by numbers written in its own profile. The other limits are
off when unset: an unset limit is not checked. A limit that is set
must be positive. Every other field has a documented default.

A field the profile doesn't know is refused, not ignored, so a
misspelt limit is an error rather than a run without it; only
`provider.params` is open. The `tools` and `mcp` tables are refused
until the capability they configure is built, so a profile never
claims what a run won't do; their rows below are the design, settled
by the change that builds each.

| Field | What it sets | Default |
|---|---|---|
| `system_prompt` | The system message, verbatim. Empty means no system message is sent. | empty |
| `limits.steps` | Most model calls in a run. | required |
| `limits.wall_s` | The run's deadline, in seconds. | required |
| `limits.cost_usd` | Stop once the reported cost passes this many US dollars. Read from the digits written, never through a float. See [Cost](#cost). | unset |
| `limits.tokens` | Stop once total tokens pass this. | unset |
| `limits.context_tokens` | Stop once a model call's prompt passes this many tokens. See [Context size](#context-size). | unset |
| `limits.tool_output_bytes` | A tool result longer than this is cut, and the cut is marked. A size cap, not a run bound, so it has a default: about 8,000 tokens of text. | 32768 |
| `provider.base_url` | The API root of any OpenAI-compatible server. See [The provider](#the-provider). | `https://openrouter.ai/api/v1` |
| `provider.api_key_env` | The environment variable holding the key, sent as a bearer token. Unset sends no key (a local server); set but empty in the environment ends the run before it starts. | unset |
| `provider.params` | Temperature, max tokens and the like, passed through as given. May not set `model`, `messages`, `tools` or `stream`. | none |
| `tools.local` | Not built. Which local tools exist: `read`, `list`, `search`, `shell`. | |
| `tools.shell_allow` | Not built. The shell's allowlist, as leading whole words (`git log`). See [The shell tool](#the-shell-tool). | |
| `tools.sandbox` | Not built. How shell commands are confined: `seatbelt` (macOS) by default; `none` must be written out. | |
| `mcp.<name>` | Not built. An MCP server: `url`, and the environment variable holding its key; optional tool allow or deny list. | |

A minimal profile for OpenRouter:

```toml
[limits]
steps = 30
wall_s = 600
cost_usd = 0.50

[provider]
api_key_env = "OPENROUTER_API_KEY"
```

## Events (proposed)

One JSON object per line, each with `type` and a run-relative time.
Each line is written and flushed when the thing it records happens, so
a caller (or `jq` in a terminal) watches the run live rather than
after it.

| `type` | Carries |
|---|---|
| `start` | Jakkals version, profile hash, model, tool names, and the limits in force. |
| `model_request` | Step, number of messages sent. Written as the request leaves, so a slow call shows as in flight and a run killed mid-call shows which call it died in. |
| `model_call` | The reply to a `model_request`: step, generation id, model and provider that served it, input/output/cached tokens, cost, duration, finish reason. |
| `tool_call` | Step, tool, arguments as the model wrote them, `status` (`ok`, `failed`, `refused`), the result's size before any cut, whether it was cut, duration. |
| `answer` | The final text. |
| `exit` | Why the run ended, as `reason`: `done`; `limit` with `which`; or `error` with a typed `error` (`{"kind":"provider","provider_error":"status","status":429,…}`, `{"kind":"cost_unreported"}`). And the totals: steps, tool calls, input and output tokens, cost. |

A run with no `exit` line did not end cleanly and is void. Costs are
in billionths of a US dollar (`cost_nano_usd`), integers, `null` where
not reported.

## Exit status

`jakkals run` exits with a status per way the run ended, so a caller
can branch without reading the events; the `exit` event says the rest.

| Status | Meaning |
|---|---|
| 0 | `done`: the model answered. |
| 2 | The run never started: a bad argument, profile, key variable or `--cwd`. The reason is on stderr, and no events are written. |
| 3 | `limit`: the `exit` event names which. |
| 4 | `error`: the `exit` event carries the typed error. |

Any other status (a panic, a kill) means the run did not end cleanly;
its events have no `exit` line.

## Survivable failures (proposed)

Only these end in something other than an error exit, each with an
event: a tool that fails (its error goes back to the model as the
result); a tool call the allowlist refuses (the refusal goes back to
the model); a limit reached (exit `limit`). Whether a provider error is
retried, and how often, is a profile choice defaulting to no.

## Context size

Jakkals never shrinks the conversation: no trimming, no summarising,
no compaction. What the model sees is every message of the run, with
tool results cut only by `limits.tool_output_bytes`, and the cut is
marked.

A profile bounds the conversation with `limits.context_tokens`. After
each model call, the loop compares that call's reported
`usage.prompt_tokens` with the budget; once it is over, the run ends
with exit `limit`, naming `context_tokens`. The count is the
provider's own, so no tokenizer is needed and it is exact for every
model; the price is that the run stops one step after the budget is
passed, never before. `limits.tokens` is a different bound: it sums
tokens over all calls and says nothing about how full the window is.

When any limit ends a run, the model gets no extra turn to wrap up: a
run that hits a limit has no answer, and that absence is the result.

With the budget unset, or set above the model's window, the provider
refuses the oversized request and the run ends in a provider error
carrying the HTTP status and the provider's body. It is typed as a
context overflow only if the provider says so in a structured field,
never by matching the message.

A declared trimming or compaction rule may become a profile option
later, off by default, with an event each time it acts. It is not
designed yet.

## The shell tool

There is no shell interpreter. A command is split into words (with
shell quoting rules) and run directly, so an allowlist entry means
what it says. A command holding a pipe, redirect, `;`, `&&`, `||`,
`$(`, a backtick or a glob is refused, and the refusal goes back to the
model with a `tool_call` event marked refused. The allowlist matches
whole leading words: `git log` allows `git log --oneline`, not
`git logfoo`. Each command runs in the working directory, with a
minimal environment, a timeout and the output cap.

Word checks can't keep a command inside the working directory:
`cat /etc/hosts` and `git -C / log` begin with allowed words. That is
the sandbox's job. `tools.sandbox` confines each command at the OS
level: no writes anywhere, no reads outside the working directory and
the system paths a program needs to start.

| Sandbox | Status |
|---|---|
| `seatbelt` | macOS, through `sandbox-exec` and a profile kept in this repository. The default. |
| `none` | Word checks only. Must be written out in the profile. |
| `landlock` | Linux. Later, when a run needs Linux. |
| `container` | Later: commands run in a throwaway container, where writes, even destructive ones, can be allowed and watched. |

The sandbox is on by default, an exception to capabilities being off
by default, because it takes power away rather than adding it. Until
`seatbelt` lands, the word checks are the only guard, and a run's
`start` event says so.

## Cost

Jakkals records the cost each reply reports (`usage.cost`) and the
reply's generation id on every `model_call`, and the sum on `exit`. It
makes no other calls for cost: a caller that wants the final billed
cost looks each generation id up with the provider afterwards, once
the figures have settled, outside the run. A reply without a cost
records `null`, never 0: a local server's run was not measured, not
free.

`limits.cost_usd` is checked after each call against the reported
sum, so like the context budget it stops a run one call late; the
hard bound is the credit limit on the provider key. With the limit
set, a reply that reports no cost ends the run with error
`cost_unreported`, since the limit could no longer be enforced.

## The provider

[`src/provider/http.rs`](../src/provider/http.rs) speaks OpenAI-compatible
chat completions. Each model call is one `POST` to
`{provider.base_url}/chat/completions`, carrying the model, the
conversation, the tools (left out when there are none) and
`provider.params` as given. The params may not set `model`, `messages`,
`tools` or `stream`, which are the run's own. A key, where the profile
names one, goes as a bearer token.

Nothing happens on the side: no retries, and no redirects (a 3xx
ends the call as a status error). The run's time left is the request's
timeout, covering connecting, sending and reading the whole reply.

| The call | Ends as |
|---|---|
| A 2xx with one completion | The reply. |
| A non-2xx status | `status`, with the body, cut at 16 KiB and marked. |
| The deadline passes | `deadline`, so the run exits `limit` `wall_s`. |
| No answer (refused, reset, TLS) | `transport`, with the cause. |
| A 2xx that isn't one completion: not JSON, an error object, no or several choices, no usage, a cost that isn't a number, a body past 16 MiB | `malformed`, with what was wrong. |

From a reply Jakkals keeps the text, the tool calls, the usage (with
cached tokens where reported), `usage.cost`, the `id` (OpenRouter's
generation id), the model and provider that served it, and the finish
reason. Other fields are dropped; a reasoning model's reasoning text
among them, so it is not sent back on the next call. The cost is read
from the number's decimal digits, never through a float, and rounded
to the nearest nano-dollar. A server that reports cost only when asked
is asked through `provider.params`.

## The dependency register

Every dependency has a row here before it enters `Cargo.toml`.

| Crate | Why | In `Cargo.toml` |
|---|---|---|
| `clap` (derive) | The CLI's parsing and help. | yes |
| `tokio` | `rmcp` and `reqwest` are async; one runtime for both. | yes |
| `reqwest` (rustls) | HTTP to the provider. | yes |
| `serde`, `serde_json` (`raw_value`) | Wire types, events; `raw_value` keeps a reported cost as the digits the server wrote. | yes |
| `toml` | The profile. | yes |
| `rmcp` | MCP client. | not yet |
| `ignore`, `grep-searcher` | The `list` and `search` tools, with ripgrep's ignore rules. | not yet |
| `sha2` | The profile hash. | yes |
| `shlex` | Splitting a shell command into words with the shell's quoting rules, without a shell. | not yet |
