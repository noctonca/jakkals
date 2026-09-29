//! Time, as the loop reads it: milliseconds since the run started.

use std::time::Instant;

pub trait Clock {
    /// Milliseconds since the run started; never goes backwards.
    fn now_ms(&self) -> u64;
}

/// The real clock: monotonic, from when it was made.
pub struct MonotonicClock {
    start: Instant,
}

impl MonotonicClock {
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        // u64 milliseconds outlast any run by millions of years, so the
        // conversion from u128 can't fail in practice.
        u64::try_from(self.start.elapsed().as_millis()).expect("run shorter than 584 million years")
    }
}
