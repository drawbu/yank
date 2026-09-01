//! Jittered exponential backoff, shared by the reconnect loops.

use std::time::Duration;

/// Returns growing delays, up to a ceiling, and goes back to the floor
/// once whatever it guards has been stable.
#[derive(Debug)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    current: Duration,
}

impl Backoff {
    pub fn new(min: Duration, max: Duration) -> Self {
        Backoff {
            min,
            max,
            current: min,
        }
    }

    /// The next delay to wait, moving the following one toward the
    /// ceiling.
    pub fn next_delay(&mut self) -> Duration {
        // Jittered by a fifth either way, so several loops backing off
        // together do not retry in lockstep.
        let delay = self.current.mul_f64(rand::random_range(0.8..1.2));
        self.current = (self.current * 2).min(self.max);

        delay
    }

    /// Returns to the floor delay.
    pub fn reset(&mut self) {
        self.current = self.min;
    }
}
