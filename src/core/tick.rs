//! Fixed 20 TPS tick clock for the play-state loop.
//!
//! The clock is a plain monotonic counter with no embedded time source, so
//! it stays trivially testable and `Send`. The async play loop
//! (`minecraft::play`) owns a real `tokio` interval and calls
//! [`TickClock::advance`] once per scheduled tick; per-tick behavior (idle
//! movement, physics) is layered on top in later phases.

use std::time::Duration;

/// Minecraft's fixed server tick rate.
pub const TICKS_PER_SECOND: u64 = 20;

/// Wall-clock duration of a single tick (50 ms at 20 TPS).
pub const TICK_DURATION: Duration = Duration::from_millis(1000 / TICKS_PER_SECOND);

/// A monotonic tick counter driven by the play loop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickClock {
    tick: u64,
}

impl TickClock {
    /// A clock that has not ticked yet (current tick 0).
    pub const fn new() -> Self {
        Self { tick: 0 }
    }

    /// The current tick number; 0 until the first [`advance`](Self::advance).
    pub const fn current(&self) -> u64 {
        self.tick
    }

    /// Advances by one tick and returns the new count.
    pub fn advance(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Whole ticks that elapse over `elapsed` at the fixed tick rate.
    ///
    /// Independent of any clock instance; useful for reasoning about
    /// durations in tick units (e.g. keep-alive cadence).
    pub const fn ticks_in(elapsed: Duration) -> u64 {
        (elapsed.as_millis() / TICK_DURATION.as_millis()) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_zero() {
        let clock = TickClock::new();
        assert_eq!(clock.current(), 0);
    }

    #[test]
    fn advance_increments_and_returns_new_count() {
        let mut clock = TickClock::new();
        assert_eq!(clock.advance(), 1);
        assert_eq!(clock.advance(), 2);
        assert_eq!(clock.current(), 2);
    }

    #[test]
    fn tick_duration_is_50ms() {
        assert_eq!(TICK_DURATION, Duration::from_millis(50));
    }

    #[test]
    fn ticks_in_counts_whole_ticks() {
        assert_eq!(TickClock::ticks_in(Duration::from_secs(1)), 20);
        assert_eq!(TickClock::ticks_in(Duration::from_millis(50)), 1);
        assert_eq!(TickClock::ticks_in(Duration::from_millis(49)), 0);
        assert_eq!(TickClock::ticks_in(Duration::from_millis(125)), 2);
    }
}
