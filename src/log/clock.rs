//! The hybrid logical clock ordering log entries.
//!
//! Machines need one agreed order over entries written concurrently, and
//! that order has to stay close to wall time: the clipboard shows a
//! history the user reads as "what I copied last", and lifetimes expire
//! against real seconds. A plain wall clock cannot do it (two machines
//! disagree by whatever their drift is, and one of them going backwards
//! reorders history), and a plain counter loses the tie to real time.
//!
//! A hybrid logical clock gives both: it advances with wall time when wall
//! time moves forward, and with a counter when it does not, so it never
//! goes backwards and never drifts far from the real seconds.
//!
//! One machine with a badly wrong clock is contained: a remote timestamp
//! more than five minutes ahead of us does not drag our own clock with it
//! (see [`Clock::observe`]).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How far ahead of local wall time a peer's timestamp is allowed to push
/// our clock. Past this the peer's clock is wrong, and adopting it would
/// park our own clock in the future for as long as the daemon runs.
const MAX_DRIFT: Duration = Duration::from_mins(5);

/// A hybrid logical clock reading.
///
/// Ordering is `millis` first, then `counter`. It is a *partial* order:
/// two machines can produce the same reading, which the log breaks by
/// entry id (see [`crate::log::Entry`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hlc {
    /// Milliseconds since the unix epoch, as far as the clock knows.
    pub millis: u64,
    /// Disambiguates events sharing a millisecond, and keeps the clock
    /// moving while wall time does not.
    pub counter: u16,
}

impl Hlc {
    /// The wall-clock instant this reading claims to be at.
    pub fn as_system_time(self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.millis)
    }
}

/// A machine's clock: the highest reading it has produced or seen.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Clock {
    last: Hlc,
}

impl Clock {
    /// Resumes from a persisted reading, so a restart (or a wall clock
    /// that jumped backwards) cannot re-issue timestamps already used.
    pub fn resume(last: Hlc) -> Self {
        Clock { last }
    }

    /// The last reading, to persist.
    pub fn last(self) -> Hlc {
        self.last
    }

    /// Stamps a local event.
    pub fn tick(&mut self) -> Hlc {
        let wall = wall_millis();
        self.last = if wall > self.last.millis {
            Hlc {
                millis: wall,
                counter: 0,
            }
        } else {
            Hlc {
                millis: self.last.millis,
                counter: self.last.counter.saturating_add(1),
            }
        };

        self.last
    }

    /// Takes a remote reading into account, so anything we stamp
    /// afterwards sorts after what we have seen.
    ///
    /// A reading further than `MAX_DRIFT` ahead of our wall clock is not
    /// adopted: the entry keeps the timestamp its author gave it (the
    /// order stays the same on every machine) but a peer whose clock is
    /// years off cannot push ours along with it.
    pub fn observe(&mut self, remote: Hlc) {
        let wall = wall_millis();
        let ceiling = wall.saturating_add(millis_of(MAX_DRIFT));
        let remote = Hlc {
            millis: remote.millis.min(ceiling),
            ..remote
        };

        let highest = self.last.max(remote);
        self.last = if wall > highest.millis {
            Hlc {
                millis: wall,
                counter: 0,
            }
        } else {
            Hlc {
                millis: highest.millis,
                counter: highest.counter.saturating_add(1),
            }
        };
    }
}

/// Milliseconds since the unix epoch, clamped at the epoch for a system
/// clock set before it.
fn wall_millis() -> u64 {
    millis_of(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default(),
    )
}

fn millis_of(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_never_repeat_or_go_backwards() {
        let mut clock = Clock::default();
        let readings: Vec<Hlc> = (0..1000).map(|_| clock.tick()).collect();

        assert!(readings.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_resumed_clock_outranks_what_it_issued() {
        let mut clock = Clock::default();
        let issued = clock.tick();

        let mut resumed = Clock::resume(issued);
        assert!(resumed.tick() > issued);
    }

    #[test]
    fn observing_a_peer_orders_us_after_it() {
        let mut clock = Clock::default();
        let ahead = Hlc {
            millis: wall_millis() + 1000,
            counter: 7,
        };

        clock.observe(ahead);
        assert!(clock.tick() > ahead);
    }

    #[test]
    fn a_peer_clock_years_ahead_does_not_drag_ours() {
        let mut clock = Clock::default();
        let broken = Hlc {
            millis: wall_millis() + millis_of(Duration::from_hours(24 * 365)),
            counter: 0,
        };

        clock.observe(broken);
        assert!(clock.tick().millis <= wall_millis() + millis_of(MAX_DRIFT) + 1);
    }
}
