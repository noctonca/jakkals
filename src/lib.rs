//! Jakkals: a small coding agent whose every model call, tool call and
//! limit is written down. See docs/ARCHITECTURE.md for the design.
//!
//! The loop ([`run::run`]) decides; the edges act. It takes a
//! [`provider::Provider`], a [`tools::Tools`], a [`clock::Clock`] and an
//! [`events::Sink`], so tests drive it with canned replies and virtual
//! time.

pub mod clock;
pub mod conversation;
pub mod events;
pub mod provider;
pub mod run;
pub mod tools;

#[cfg(test)]
mod scripted;
