# Working on Jakkals

For anyone changing Jakkals, people and coding assistants alike. The
architecture is [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md); the coding
discipline is [TIGERSTYLE.md](TIGERSTYLE.md). Read both before touching
`src/`.

The bar is the same for everyone, AI-assisted or not: the code is
correct, the PR's scope matches what it claims, and nothing the
harness does is hidden. If an assistant wrote it, you are still the
author: read the diff, and be able to defend every line.

## Principles

- **Nothing hidden.** Everything the harness does between the prompt and
  the answer is either in the profile or in the event stream. A
  behaviour that is neither (a silent retry, an injected turn, a
  fallback model, a trimmed context) is a bug, however helpful it looks.
- **The profile is the harness.** System prompt, tools, limits and
  context size come from the profile, and a run records the profile's
  hash. A default that changes behaviour is a profile field with its
  value written in the docs, not a constant in the loop.
- **Capabilities are toggles.** A new behaviour (trimming, retries,
  a wrap-up turn) arrives as a profile field, off by default, with an
  event each time it acts. It earns its place when a run needs it,
  not because a feature list has it: guard against sprawl.
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

`cargo test` passing is necessary, not sufficient: before a PR that
changes what a run does, run it against a real model on a small,
invented task and read the events. Keep real-run profiles in
`*.local.toml` files, which git ignores.

Each system's sandbox tests run only on that system; CI runs both.
To run the Linux ones from a Mac, in Docker:

```sh
docker run --rm -v "$PWD":/src:ro -v jakkals-target:/target \
  -v jakkals-cargo:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/target \
  -w /src rust:latest cargo test --locked
```

The hooks in `scripts/git-hooks` check staged files against the
rustfmt policy and refuse private network addresses and home paths;
turn them on once per clone with
`git config core.hooksPath scripts/git-hooks`. The `commit-msg` hook
also refuses messages matching patterns you keep outside the repo;
its header says how.

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
- **A CLI option** → its `--help` text, the README when it changes how
  most people run Jakkals, and ARCHITECTURE.md when it changes what a
  run records.
- **Anything with a limit, timeout, retry or capacity** → name it, with
  a comment saying where its value comes from.

## Keeping the docs true

The docs are part of the change. A PR that changes behaviour, a
profile field, an event, an exit status, a bound, a CLI option or a
dependency updates the docs in the same PR. Its description names the
sections it touched, or says why none needed to change.

Each fact has one home, and the other docs link to it:

| Doc | Holds |
|---|---|
| [README.md](README.md) | What Jakkals is, how to install and run it, one example. |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | The design and the reference: profile fields, events, the transcript, exit statuses, tools, bounds, the dependency register. |
| [TIGERSTYLE.md](TIGERSTYLE.md) | How the code is written. |
| AGENTS.md | How to work here. |
| `--help` and doc comments | The CLI's own text, and why the code is as it is. |

Keep them clean:

- Write what is true now, in the present tense. Delete what stopped
  being true rather than adding a correction next to it; git keeps the
  history. Nothing stays marked "proposed" once it is built.
- Work not built yet is marked **Later** in ARCHITECTURE.md, or goes
  in `docs/plan/` if it needs more than a line.
- Before opening a PR, search the docs for every name the change adds,
  renames or removes, and check the README's example still runs.
- Plain words, short sentences, sentence-case headings. Link from
  `docs/` to source with relative paths.

## Commit & PR conventions

Conventional Commits, CI-enforced on PR titles: `feat`, `fix`, `perf`,
`refactor`, `docs`, `test`, `build`, `ci`, `chore`. Commit messages and
PR bodies explain *why* — the constraint hit, the alternative rejected.
Keep PRs to one topic; resist "while I'm here" cleanups, and open an
issue before anything sweeping. When CI fails, fix the cause.

A bug or an idea is an issue. For a run that went wrong, say which
model and attach the event lines around the failure, with any private
content removed.
