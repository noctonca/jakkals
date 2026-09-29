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

Early. `jakkals run` works end to end against OpenRouter or a local
server, with the local tools `read`, `list`, `search` and an
allowlisted `shell` (sandboxed on macOS and Linux) when the profile
offers them, and the tools it lists from MCP servers over streamable
HTTP. The design is [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

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
cargo run -- run --profile openrouter.local.toml --model <provider/model> \
  --cwd <project> --prompt "<task>" | jq -c .
```

The events go to stdout as JSON lines and the exit status says how the
run ended; both are described in the architecture.

## Licence

MIT OR Apache-2.0, at your option.
