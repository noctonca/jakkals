# Agent guidance — Jakkals

Project context for AI assistants (and a fine crib for humans). The
architecture is [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md); the coding
discipline is [TIGERSTYLE.md](TIGERSTYLE.md). Read both before touching
`src/`.

## Principles

- **Nothing hidden.** Everything the harness does between the prompt and
  the answer is either in the profile or in the event stream. A
  behaviour that is neither (a silent retry, an injected turn, a
  fallback model, a trimmed context) is a bug, however helpful it looks.
- **The profile is the harness.** System prompt, tools, limits and
  context size come from the profile, and a run records the profile's
  hash. A default that changes behaviour is a profile field with its
  value written in the docs, not a constant in the loop.
- **Small enough to read.** The loop fits in one sitting. Prefer a
  plain function to a framework, and our own 80 lines to a crate whose
  surface we use 2% of.
- **The design is written down.** If a change contradicts
  ARCHITECTURE.md, the doc changes first (or the change is wrong).

## Never

- Never add a dependency without its row in ARCHITECTURE.md's
  dependency register.
- Never retry, re-prompt, switch models or edit the conversation unless
  the profile says so, and never without an event saying it happened.
- Never hard-code a limit; name it, and make it a profile field when
  it changes what a run can do.
- Never let a tool reach outside the run's working directory, or run a
  shell command the profile's allowlist doesn't name.
- Never classify an error by string matching.
- Never commit secrets, keys, hostnames, LAN addresses or anything
  specific to one person's setup: this repo is public. Endpoints and
  keys come from the environment or the profile at run time.
- Never cite what a reader can't see (private notes, a past working
  session, a numbered experiment) in a comment, doc or commit message;
  state the fact itself.
- Never put a transcript, a prompt's real content or a model's answer
  from a real run into the repo; tests use invented material.
- Never skip hooks or CI (`--no-verify`).

## Build & test

```sh
cargo check                  # green on any dev machine
cargo test                   # no network, no API key
cargo clippy --all-targets   # -D warnings in CI
cargo fmt --all
```

Tests that need a model or an MCP server talk to a local fake, never
to the network.

## When modifying X, do Y

- **The loop** → extend the scripted-provider tests alongside: a fake
  provider replays canned replies, and the test asserts the exact
  events and the exit.
- **An event's shape** → it is the interface other tools read: update
  the event reference in ARCHITECTURE.md in the same commit, and add a
  field rather than change one.
- **A profile field** → document it and its default in ARCHITECTURE.md;
  a run with the old profile must still mean what it meant.
- **A tool** → its description is part of the prompt: keep it short,
  and test what it refuses as well as what it does.
- **Anything with a limit, timeout, retry or capacity** → name it, with
  a comment saying where its value comes from.
- **Docs** → sentence-case headings; link from `docs/` to source with
  relative paths; a plan for future work goes in `docs/plan/`.

## Commit & PR conventions

Conventional Commits, CI-enforced on PR titles: `feat`, `fix`, `perf`,
`refactor`, `docs`, `test`, `build`, `ci`, `chore`. Commit messages and
PR bodies explain *why* — the constraint hit, the alternative rejected.
Keep PRs to one topic.
