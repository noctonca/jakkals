# How to contribute

Jakkals is young — one maintainer, a written design, and no release yet.
Contributions are welcome, and the bar is the same for everyone
(AI-assisted or not): the code has to be correct, the scope has to
match what the PR claims, and nothing the harness does may be hidden.

Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and
[TIGERSTYLE.md](TIGERSTYLE.md) first; point your assistant at
[AGENTS.md](AGENTS.md).

## Doing good work here

- **Own what you ship.** If an assistant wrote it, you are still the
  author: read the diff, be able to defend every line.
- **Actually test your change.** Build it, run it against a real model
  on a small task, and read the event stream. `cargo test` passing is
  necessary, not sufficient.
- **Ship a test.** Loop behaviour gets a scripted-provider test; a tool
  gets tests for what it does and what it refuses.
- **Smaller is better.** One topic per PR; resist "while I'm here"
  cleanups. Anything sweeping deserves an issue first.
- **Explain why, not what.** In commits, PRs and comments alike — the
  diff already shows the what.
- **CI is on your side.** When it fails, fix the cause; don't skip a
  hook or disable a check to unblock yourself.

## Conventions

PR titles follow Conventional Commits (`feat`, `fix`, `perf`,
`refactor`, `docs`, `test`, `build`, `ci`, `chore`) — CI checks this.
`cargo fmt` and clippy (`-D warnings`) gate merges. A local pre-commit
hook checks the staged blobs against the same rustfmt policy, and
refuses private network addresses and home paths — opt in with
`git config core.hooksPath scripts/git-hooks`.

## Bugs and ideas

Open an issue. For a run that went wrong, say which model and attach
the event lines around the failure, with any private content removed.
