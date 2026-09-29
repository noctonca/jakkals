# Architecture

This is the design and the reference: what each profile field, event,
exit status and bound means. It describes what the code does now;
work not built yet is marked **Later**.

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
   ├──────────────── events (stdout, JSON lines) ──▶ caller
   └──────────────── transcript (a file, JSON lines), when asked
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
`provider.params` is open. A shell setting when `shell` isn't offered
is refused too, so a profile never claims what a run won't do.

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
| `tools.local` | The local tools offered, by name: `read`, `list`, `search`, `shell`. Offered in that order whatever the order written; a name twice is refused. See [The local tools](#the-local-tools) and [The shell tool](#the-shell-tool). | none |
| `tools.shell_allow` | The commands `shell` may run, each as its leading words (`git log`). Required when `shell` is offered. | none |
| `tools.sandbox` | How shell commands are confined: `seatbelt` (macOS), `landlock` (Linux) or `none`, which must be written out. | the system's own: `seatbelt` on macOS, `landlock` on Linux |
| `tools.sandbox_read` | Absolute paths the sandbox also lets commands read, for programs and their libraries installed outside the system's own paths (a package manager's prefix). | none |
| `tools.shell_timeout_s` | Seconds a shell command may run before it is killed. | 30 |
| `mcp.<name>.url` | An MCP server's streamable HTTP endpoint. `<name>` is 1 to 16 of `a-z`, `0-9` and `-`, and prefixes its tools' names. See [MCP servers](#mcp-servers). | required |
| `mcp.<name>.tools` | The server's tools offered, by the server's names, in the order written; a name twice is refused. | required |
| `mcp.<name>.key_env` | The environment variable holding the server's key. Unset sends no key; set but empty in the environment stops the run before it starts. | unset |
| `mcp.<name>.key_header` | The header the key goes in. `authorization` sends it as a bearer token; any other name sends it as it is. Only with `key_env`. | `authorization` |
| `mcp.<name>.call_timeout_s` | Seconds one call to the server's tools may run before it fails. | 60 |

A minimal profile for OpenRouter:

```toml
[limits]
steps = 30
wall_s = 600
cost_usd = 0.50

[provider]
api_key_env = "OPENROUTER_API_KEY"
```

## Events

[`src/events.rs`](../src/events.rs). One JSON object per line on
stdout, each with its `type` and `t_ms`, the milliseconds since the
run started. Each line is written and flushed when the thing it
records happens, so a caller (or `jq` in a terminal) watches the run
live rather than after it.

The events are the interface other programs read: a field is added,
never renamed, retyped or removed. Costs are integers in billionths
of a US dollar (`cost_nano_usd`); a field the provider or server
didn't report is `null`, never 0. Steps count from 1.

A run writes `start`, then one `mcp_server` per server, then for each
step a `model_request`, its `model_call` and the step's `tool_call`s,
then `answer` if the model answered, and `exit` last. A run with no
`exit` line did not end cleanly and is void.

| `type` | Fields |
|---|---|
| `start` | `version` (Jakkals's); `profile_hash`; `model` (as asked for); `tools`, the names offered in order, MCP tools with their prefix; `sandbox`, the shell's (`seatbelt`, `landlock`, `none`, or `null` when `shell` isn't offered); `limits`, those in force: `steps`, `wall_s`, `cost_nano_usd` (the profile's `cost_usd` in nano-dollars), `tokens`, `context_tokens`, `tool_output_bytes`, unset ones `null`; `transcript`, the file's absolute path, or `null` when none is written. |
| `mcp_server` | One per MCP server, in the order its tools are offered. `server`, the profile's name for it; `server_name` and `server_version`, as the server reports them; `protocol_version`, the one agreed; `setup_ms`; `tools`, each offered tool exactly as the model sees it: `name`, `description`, `parameters` (a JSON Schema). |
| `model_request` | `step`; `messages`, the number sent. Written as the request leaves, so a slow call shows as in flight and a run killed mid-call shows which call it died in. |
| `model_call` | The reply to the `model_request` of the same `step`: `generation_id`; `model` and `provider`, those that served it (a router may pick another model than the one asked for); `input_tokens`, `output_tokens`; `cached_tokens`; `reasoning_tokens` (counted in `output_tokens`, not on top of them); `cost_nano_usd`; `duration_ms`; `finish_reason`. A call that fails has no `model_call`: the `exit` carries the error. |
| `tool_call` | `step`; `tool`; `arguments`, a string exactly as the model wrote them, usually JSON; `status`: `ok`, `failed` (the tool ran and it failed) or `refused` (it never ran: a tool not offered, a path outside `--cwd`, a command the allowlist doesn't name); `result_bytes`, the result's size before any cut; `cut`; `duration_ms`. |
| `answer` | `text`, the reply that ended the run, `""` when it had no text. Only when the run ends `done`. |
| `exit` | `reason`: `done`; `limit`, with `which` naming the limit as the profile does (`steps`, `wall_s`, `cost_usd`, `tokens`, `context_tokens`); or `error`, with a typed `error`, below. And `totals`: `steps`, `tool_calls`, `input_tokens`, `output_tokens`, `cost_nano_usd` (`null` once any reply reported no cost, since a partial sum would understate the run). |

An `error` is one of:

| `error` | Meaning |
|---|---|
| `{"kind":"provider","provider_error":"status","status":429,"body":"…"}` | The provider answered with a status other than 2xx. |
| `{"kind":"provider","provider_error":"transport","detail":"…"}` | No answer: refused, reset, TLS. |
| `{"kind":"provider","provider_error":"malformed","detail":"…"}` | A 2xx that isn't one completion. See [The provider](#the-provider). |
| `{"kind":"cost_unreported"}` | `limits.cost_usd` is set and a reply reported no cost. |

A provider that runs out of time is not an error: the run ends
`limit` `wall_s`.

A short run, with the `start` and `model_call` lines trimmed:

```jsonl
{"type":"start","version":"0.0.0","profile_hash":"sha256:…","model":"some/model","tools":["read","list"],"sandbox":null,"limits":{…},"transcript":null,"t_ms":0}
{"type":"model_request","step":1,"messages":2,"t_ms":1}
{"type":"model_call","step":1,"generation_id":"gen-…","input_tokens":812,"output_tokens":41,"cost_nano_usd":93000,"finish_reason":"tool_calls",…,"t_ms":1650}
{"type":"tool_call","step":1,"tool":"read","arguments":"{\"path\":\"box.txt\"}","status":"ok","result_bytes":28,"cut":false,"duration_ms":0,"t_ms":1651}
{"type":"model_request","step":2,"messages":4,"t_ms":1651}
{"type":"model_call","step":2,…,"finish_reason":"stop","t_ms":2903}
{"type":"answer","text":"The box holds three red marbles.","t_ms":2903}
{"type":"exit","reason":"done","totals":{"steps":2,"tool_calls":1,"input_tokens":1690,"output_tokens":63,"cost_nano_usd":191000},"t_ms":2903}
```

## The transcript

The events say what happened and what it cost, but not what was said:
a tool call's result and a reply's text (other than the answer) are
not in them. A caller that needs to read the run, to judge whether an
answer rests on what the model saw or to see why a run went wrong,
asks for a transcript with `--transcript`. It is off by default.

`--transcript` on its own writes the file to
`$XDG_DATA_HOME/jakkals/transcripts/`, or
`~/.local/share/jakkals/transcripts/` when that variable is unset or not an absolute path (as the XDG spec asks),
on macOS as on Linux. The file is named for the time the run started,
in UTC, and the process id, `2026-01-31T14-05-09Z-4242.jsonl`, so the
names sort by time, stay unique, and carry no colon. A folder the
option has to make is created readable by its owner only (mode 700).
`--transcript <path>` writes to that path instead, for a caller that
names its own files. Either way the `start` event carries the file's
absolute path, so a reader of the events can find it.

It is a command-line option, not a profile field, because it records
the run without changing it: the model sees the same conversation
either way, so two runs with the same profile stay comparable, and
the profile hash doesn't move.

The file is the conversation, one JSON object per line, each with its
`role`, the `step` it belongs to (0 before the first model call) and
the run-relative `t_ms`:

| `role` | Carries |
|---|---|
| `system` | The system prompt, when there is one. |
| `user` | The task. |
| `assistant` | One model reply: its text (or `null`), its tool calls (id, name, arguments as written) and, with `--transcript-reasoning`, its reasoning text (or `null` when the reply carries none). |
| `tool` | One tool result: the call's id and tool name, and the text exactly as the model got it, cut by `limits.tool_output_bytes` and marked. |

Every reply is written, including the one that ends the run, whether
it is the answer or a reply whose tool calls a limit kept from
running. A tool call left unrun has no `tool` line, as it has no
`tool_call` event. So the lines are what the model was sent and what
it wrote, in order: a `model_request` event's `messages` count is the
number of `system`, `user`, `assistant` and `tool` lines before it.

Each line is written and flushed as it happens, like the events, so a
run killed part-way leaves the transcript up to that point. A line
that can't be written ends the process as an unwritable event stream
does: a transcript with a gap would mislead.

A reasoning model's reasoning text is left out unless
`--transcript-reasoning` is given (it needs `--transcript`). It can
be long, and it is not part of what the model is sent, so it is
opt-in. Where the provider returns no reasoning text (some hide it),
the line says `null` and the `model_call` event's `reasoning_tokens`
remains its only trace.

A transcript holds everything the model saw: file contents, command
output, MCP results. So the file is created new, readable and
writable by its owner only (mode 600), and a path that already
exists is refused, so no run writes over another's record or into a
file someone else set up. It is created last in setup, after the MCP
servers are connected, so a run that fails setup leaves no empty file
to block its retry. Where it lives is the caller's choice. A
caller running several runs should keep their transcripts outside
every run's `--cwd` and `tools.sandbox_read`, so a model can't read
another run's.

## Exit status

`jakkals run` exits with a status per way the run ended, so a caller
can branch without reading the events; the `exit` event says the rest.

| Status | Meaning |
|---|---|
| 0 | `done`: the model answered. |
| 2 | The run never started: a bad argument, profile, key variable or `--cwd`, a transcript file that couldn't be created, or an MCP server that couldn't be set up. The reason is on stderr, and no events are written. |
| 3 | `limit`: the `exit` event names which. |
| 4 | `error`: the `exit` event carries the typed error. |

Any other status (a panic, a kill) means the run did not end cleanly;
its events have no `exit` line.

## Survivable failures

A run goes on after only two kinds of failure, each with a `tool_call`
event: a tool call that fails, and one that is refused. Either way the
reason goes back to the model as the call's result. A limit reached
ends the run as `limit`. Anything else ends it as `error`: nothing is
retried. A retry, if one is ever wanted, would be a profile field, off
by default, with an event each time it acts (**Later**, not designed).

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

## The local tools

[`src/tools/local.rs`](../src/tools/local.rs). Each tool works on paths
relative to `--cwd`, which is resolved once, symlinks and all, before
the run starts. A path is refused before anything is read when it is
absolute or holds `..`, and after resolving when it leads outside the
directory through a symlink. A refusal is a `refused` tool call; a
missing file, bad arguments or unreadable text are `failed`. Either
way the reason goes back to the model.

| Tool | Arguments | Returns |
|---|---|---|
| `read` | `path`; optional `start_line` (from 1) and `line_count` | The file's text, or those lines of it. |
| `list` | optional `path` (default the whole directory) and `depth` (1 is the directory's own entries) | One path per line, relative to `--cwd`, sorted; directories end in `/` and symlinks in `@`. |
| `search` | `pattern`, a regular expression in Rust's `regex` syntax; optional `path` | `path:line:text` for each matching line. |

`list` and `search` walk the path they are given, skipping hidden
entries and what the `.gitignore` and `.ignore` files in it and below
ignore, as ripgrep does, in a git repository or not. Ignore files above
that path and git's global ones are not read, so what a tool sees
doesn't depend on the machine or on where `--cwd` sits; a path named
outright is walked even when hidden. They don't follow symlinks, and
`search` skips binary files (any holding a NUL byte). An entry they
can't read is skipped, and a line at the end says how many were.

Each has a bound of its own, fixed by the Jakkals version, so a call
stays cheap in time and memory even when `limits.tool_output_bytes`
would cut its result anyway:

| Bound | Value | When passed |
|---|---|---|
| `read`'s file size | 1 MiB | The call fails, naming the size; `search` reads any size. |
| `list`'s entries | 1,000 | The list stops, with a line saying so. |
| `search`'s hits | 200 | The search stops, with a line saying so. |
| `search`'s line length | 500 bytes shown, 1 MiB searched | A longer hit is shown cut, marked `…`; a file with a longer line is skipped. |

A call that is still walking when the run's deadline passes stops and
fails, and the run then ends on `wall_s`.

## The shell tool

[`src/tools/shell.rs`](../src/tools/shell.rs). `shell` takes one
argument, `command`. There is no shell interpreter: the command is
split into words with shell quoting rules and run directly, so an
allowlist entry means what it says.

A command is refused, before anything runs, when it holds any of
`|` `&` `;` `<` `>` `` ` `` `$` `*` `?` `[` or a line break, quoted or
not: no shell reads them, and a model writing one expects a shell that
isn't there. It is refused too when its words don't begin with one of
`tools.shell_allow`'s entries, matched as whole words: `git log` allows
`git log --oneline`, not `git logfoo` nor `git -C / log`. An entry
holding one of those characters is refused in the profile.

Each command runs in the working directory with no input, and an
environment holding only `PATH`, from Jakkals's own: no `HOME`, so no
user configuration is read. It runs in a process group of its own,
killed whole when `tools.shell_timeout_s` or the run's deadline
passes, whichever comes first, and again when the command ends, so
nothing it started outlives it.

The result is the command's standard output, then its standard error
after a `[jakkals: stderr]` line if there is any, then
`[jakkals: exit status N]` unless the status is 0. A status other than
0, a kill or a timeout is a `failed` tool call, and the output still
goes back to the model. Each stream is kept up to 1 MiB, a fixed bound
so a runaway command can't fill memory; what is past it is counted and
dropped, and a line says so. `limits.tool_output_bytes` then cuts the
whole as for any tool.

Word checks can't keep a command inside the working directory:
`cat /etc/hosts` begins with an allowed word. That is the sandbox's
job. `tools.sandbox` confines each command at the OS level.

| Sandbox | Status |
|---|---|
| `seatbelt` | macOS, through `sandbox-exec` and [`src/tools/shell/seatbelt.sb`](../src/tools/shell/seatbelt.sb). The default there. |
| `landlock` | Linux on x86_64 or aarch64, through Landlock and a seccomp filter: [`src/tools/shell/landlock.rs`](../src/tools/shell/landlock.rs). The default there. |
| `none` | Word checks only. Must be written out in the profile. |
| `container` | Later: commands run in a throwaway container, where writes, even destructive ones, can be allowed and watched. |

Both sandboxes draw the same line. A command may read the working
directory, the paths in `tools.sandbox_read`, and what a program needs
to start: the system's own directories, never `/usr/local`, where
package managers install. It may read any file's metadata (size and
times, not content), since programs resolve paths through their
parents. It may write nothing but `/dev/null`, and it has no network.
A denied read or write is the program's own error, in its output. A
program installed elsewhere, such as Homebrew's under `/opt/homebrew`
or Rust's under `~/.cargo`, can't even start until its prefix is in
`tools.sandbox_read`. Asking for a sandbox the system lacks stops the
run before it starts.

Under `seatbelt` the system's directories are `/bin`, `/sbin`, `/usr`,
`/System`, the loader's cache and the time zones; a denial reads
`Operation not permitted`. Other processes can't be signalled.

Under `landlock` they are `/bin`, `/sbin`, the `/lib` directories,
`/usr`, the loader's cache and configuration and `/etc/localtime`, plus
reading `/dev/zero`, `/dev/random` and `/dev/urandom`; `/proc` is not
among them, so another process's command line and environment stay
unread. A denied file reads `Permission denied`. The kernel must offer
Landlock ABI 3 (Linux 6.2) or later, the first that stops every write,
truncating included; Jakkals uses that ABI's rules and no later ones,
so a command is confined the same on every kernel it runs on. The
network goes by a seccomp filter instead, since Landlock's own network
rules cover TCP only: no socket of any kind can be opened (`Operation
not permitted`), which also keeps a command from any daemon listening
on a Unix socket, and neither can io_uring, which could open one
without the call. One gap against `seatbelt`: Landlock can't stop a
command signalling the user's other processes until ABI 6, so under
`landlock` an allowlisted `kill` could.

The sandbox is on by default, an exception to capabilities being off
by default, because it takes power away rather than adding it. A run's
`start` event names the sandbox in force.

## MCP servers

[`src/tools/mcp.rs`](../src/tools/mcp.rs). Each `mcp.<name>` table
names one server, reached over MCP's streamable HTTP transport through
`rmcp`.
A server run as a local process (MCP's stdio transport) is not
offered: it would run outside the shell's sandbox, with the user's
whole environment.

```toml
[mcp.notes]
url = "https://notes.example/mcp"
tools = ["search", "read_note"]
key_env = "NOTES_KEY"
key_header = "x-api-key"
```

The model sees each tool as `<name>_<tool>`, `notes_search` above,
so an MCP tool never shares a name with a local one, whose names hold
no `_`, nor with another server's, since a server's name holds no `_`
either. A name the provider can't take (past 64 characters, or holding
anything but letters, digits, `_` and `-`) is refused in the profile.
Local tools are offered first,
then each server's in the order its `tools` lists them, servers
ordered by name. A `key_header` naming a header the transport sets
itself (`accept`, `content-type`, `mcp-session-id`,
`mcp-protocol-version`, `last-event-id`) is refused in the profile.

The tools offered are the ones the profile lists, never simply what
the server has: a server that gains a tool can't change what a run
with the same profile offers. What the profile can't pin is each
tool's description and parameters, which the server writes and which
are part of the prompt; the `mcp_server` event records them in full,
exactly as sent to the model.

**Setting up.** Before the run's first event, Jakkals connects to
each server, completes MCP's initialization and reads its tool list.
A server that doesn't answer, refuses the key, fails either step or
lacks a tool the profile lists stops the run before it starts (exit
2), naming the server and the cause. Jakkals declares no client
capabilities, so a server can't ask it for anything (a model call,
the user's input, a list of roots); a request it sends anyway gets
MCP's method-not-found error, where `rmcp` on its own would answer a
roots request with an empty list and decline a request for input. The tool list is read once: a server
announcing a changed list is not re-read. The server's own
notifications, its log lines among them, are dropped.

**A call.** The model's arguments go to the server as it wrote them;
the server checks them against its schema. The result is the text
of the result's text parts, one after another. A result with none
but with structured content returns that content as JSON; any other
part (an image, audio, a resource) is not passed on, and a
`[jakkals: <kind> left out]` line stands in its place. A result the
server marks `isError` is a `failed` tool call, and so are an MCP
error reply (carrying its code and message), a lost connection, a
reply asking for the user's input or turning the call into a task to
poll, and a call still running when `call_timeout_s` or the run's
deadline passes, whichever comes first; a call that runs out of time
is cancelled with MCP's cancellation notice. Each goes back to the
model, and the run goes on.

**Nothing on the side.** No retries, no reconnecting, no redirects,
no new session when the server says the old one expired, no cached
tool list, and no call sent twice: `rmcp`'s client does each of these
by default or in its convenience calls, so Jakkals turns them off and
sends `tools/list` and `tools/call` as plain requests. A lost
connection is not opened again.

Fixed by the Jakkals version, as the local tools' bounds are:

| Bound | Value | When passed |
|---|---|---|
| Setting up one server | 10 s | The run doesn't start. |
| Pages of one server's tool list | 16 | The run doesn't start. |
| One streamed message (a server-sent event) | 1 MiB | The call fails. |
| Sending the cancellation of a call past its time | 2 s | Given up on; the call has failed already, and its time includes this. |

One gap: a server that answers a call with a plain JSON body rather
than a stream has that body read whole, since `rmcp` reads it so.
Only servers the profile names are reached, so the gap is bounded by
trusting them, not by Jakkals.

A key over `http://` travels in clear; that suits a server on a
private network only.

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
cached and reasoning tokens where reported), `usage.cost`, the `id`
(OpenRouter's generation id), the model and provider that served it,
and the finish reason, and a reasoning model's reasoning text where
the reply carries it (`reasoning`, or `reasoning_content` as
llama.cpp and LM Studio name it). Other fields are dropped. The
reasoning text is never sent back on the next call; it goes only to
the transcript, when the run records it (see
[The transcript](#the-transcript)), and otherwise its token count is
its only trace. The cost is read
from the number's decimal digits, never through a float, and rounded
to the nearest nano-dollar. A server that reports cost only when asked
is asked through `provider.params`.

## The dependency register

Every dependency has a row here before it enters `Cargo.toml`.

| Crate | Why | In `Cargo.toml` |
|---|---|---|
| `clap` (derive) | The CLI's parsing and help. | yes |
| `tokio` | `rmcp` and `reqwest` are async; one runtime for both. | yes |
| `reqwest` (rustls) | HTTP to the provider, and under `rmcp` to MCP servers. | yes |
| `serde`, `serde_json` (`raw_value`) | Wire types, events; `raw_value` keeps a reported cost as the digits the server wrote. | yes |
| `toml` | The profile. | yes |
| `rmcp` (`client`, `transport-streamable-http-client-reqwest`; no default features) | MCP client over streamable HTTP, on our own `reqwest` client. Default features left out: they are the server side. | yes |
| `ignore` | The `list` and `search` tools' walk, with ripgrep's ignore rules. | yes |
| `grep-searcher`, `grep-regex` | The `search` tool: ripgrep's line searcher (binary detection, bounded line buffer) and its regex matcher. | yes |
| `sha2` | The profile hash. | yes |
| `shlex` | Splitting a shell command into words with the shell's quoting rules, without a shell. | yes |
| `libc` | Killing a timed-out command's whole process group (`killpg`), and the Landlock and seccomp calls, which `std` lacks. | yes |
