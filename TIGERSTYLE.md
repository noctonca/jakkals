# TigerStyle, for Jakkals

Adapted from [TigerBeetle's TigerStyle](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md)
for an agent that spends someone's money on every step and runs with
nobody watching.

## Safety

- **End a run cleanly, or crash.** A run ends with one exit event
  naming why: done, a limit hit, or a typed error. A state
  the code doesn't understand is a panic, not a guess; the caller sees
  a missing exit event and knows the run is void.
- **Recover only where recovery is designed.** The survivable failures
  are enumerated in ARCHITECTURE.md, each with an event. Everything
  else ends the run.
- **Assert liberally.** Function entry preconditions, loop state,
  token and cost arithmetic. Assertions stay on in release.
- **`lock().unwrap()` is the policy**, not a smell: a poisoned lock is
  a crashed invariant.
- **Typed errors.** Error enums per fallible subsystem (provider, MCP,
  tools, profile). Classifying an error by matching a string is a
  review failure.
- **Explicit integer sizes** at every seam: `u32`, `u64`, never
  `usize` for anything that isn't an index into memory. Money is
  never a float in our arithmetic: cost is kept as the provider
  reports it and summed in integer nano-dollars (billionths): a
  cheap model's call can cost less than a millionth of a dollar, so
  micro-units would round it away.

## Named limits

- **Every limit has a name**, with a comment saying where its value
  comes from. No bare numbers in logic: not a timeout, a step cap, a
  byte cap on a tool's output, a poll interval.
- **Everything is bounded.** Every loop has a step cap, every request a
  deadline, every tool output a size cap, every retry (where the
  profile allows one) a count. An unbounded anything is a bug that
  hasn't fired yet.

## The loop stays pure

- The loop decides; the edges act. The loop takes a provider, a tool
  set, a clock and an event sink as traits, so a test can drive it
  with canned replies and virtual time, deterministically, with no
  network.
- Wire types (OpenRouter's JSON, MCP's messages) stay at the edges and
  are turned into our own types before the loop sees them.

## Testing

- **Scripted first.** New loop behaviour lands with a test that feeds
  it canned model replies and asserts the exact events and the exit,
  failure paths included.
- **The suite runs offline.** `cargo test` never needs a network, a
  key or a model.

## Dependencies

- Every dependency earns a row in ARCHITECTURE.md's register before it
  enters `Cargo.toml`. Prefer 80 lines of our own code to a crate
  whose surface we use 2% of.

## Style

- `cargo fmt` settles formatting arguments; clippy runs with
  `-D warnings`.
- Comments say *why*, not *what*. A constant's comment names its
  source; a workaround's comment names what it works around and when
  it can go.
- Naming: plain words, no abbreviations that save two letters
  (`deadline`, not `dl`). Units in names where a type doesn't carry
  them (`timeout_ms`, `cap_bytes`).
