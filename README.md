# Jakkals

**Jakkals** (YAH-kuls, Afrikaans for "jackal") is a small coding agent
whose every model call, tool call and limit is written down. It is
named for the jackal of the *Jakkals en Wolf* folk tales, who gets the
job done by his wits while Wolf does the heavy lifting.

Most coding agents are large, and much of what they do between your
prompt and the model's answer is hidden: a long system prompt, tools
you didn't choose, retries and extra turns you never see. That makes it
hard to tell what a model is good at from what its harness is good at.
Jakkals keeps the harness small and visible:

- **One profile holds the harness.** The system prompt, the tool list,
  the limits and the context size live in one TOML file. A run records
  the profile it used, so two runs differ only where their profiles do.
- **Nothing happens that the profile didn't ask for.** No hidden
  retries, no injected turns, no fallback models.
- **Every step is an event.** Each model call (with its generation id,
  tokens and cost) and each tool call is a JSON line on stdout.
- **Everything is bounded.** Steps, tokens, cost and time each have a
  cap, and a run that hits one says which.

It talks to [OpenRouter](https://openrouter.ai) and to any server that
speaks the OpenAI-compatible chat completions API, such as a local LM
Studio. Its tools are a handful of local ones (read, list, search, an
allowlisted shell) plus the tools it names from MCP servers.

## Status

Early, with no release yet. `jakkals run` works end to end against
OpenRouter or a local server, with the local tools `read`, `list`,
`search` and an allowlisted `shell` (sandboxed on macOS and Linux)
when the profile offers them, and the tools it lists from MCP servers
over streamable HTTP.

## Installing it

From a clone, with Rust 1.95 or later:

```sh
cargo install --path .
```

## Running it

A profile, say `openrouter.local.toml`:

```toml
[limits]
steps = 30
wall_s = 600
cost_usd = 0.50

[provider]
api_key_env = "OPENROUTER_API_KEY"

[tools]
local = ["read", "list", "search"]
```

Then:

```sh
export OPENROUTER_API_KEY=…
jakkals run --profile openrouter.local.toml --model <provider/model> \
  --cwd <project> --prompt "<task>" | jq -c .
```

The events go to stdout as JSON lines, and the exit status says how
the run ended: 0 answered, 2 never started (the reason is on stderr),
3 hit a limit, 4 an error. The events carry no conversation text
beyond the answer: add `--transcript` to record every message the
model was sent and wrote, in `~/.local/share/jakkals/transcripts/`
(or give it a file path), and `--transcript-reasoning` to include its
reasoning text.

## The reference

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) is the design, and the
reference for:

- [the profile's fields](docs/ARCHITECTURE.md#the-profile), with their defaults;
- [the events](docs/ARCHITECTURE.md#events), field by field;
- [the transcript](docs/ARCHITECTURE.md#the-transcript);
- [the exit statuses](docs/ARCHITECTURE.md#exit-status);
- [the local tools](docs/ARCHITECTURE.md#the-local-tools), [the shell and its sandboxes](docs/ARCHITECTURE.md#the-shell-tool), and [MCP servers](docs/ARCHITECTURE.md#mcp-servers).

To work on Jakkals, read [AGENTS.md](AGENTS.md): it is for people and
their coding assistants alike.

## Licence

MIT OR Apache-2.0, at your option.
