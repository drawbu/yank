//! The order machines agree on between copies.
//!
//! The latest copy wins everywhere, so every machine has to agree on what
//! "latest" means, and that order has to stay close to wall time: it is
//! what the user means by "what I copied last". A plain wall clock cannot
//! do it alone: one going backwards would let an older copy win. So a copy
//! is stamped with wall time, or a millisecond past the clip it replaces
//! when wall time is behind it, and a machine that has seen a copy always
//! stamps the next one after it.
//!
//! A machine with a badly wrong clock is contained: a stamp more than five
//! minutes ahead of us is refused (see [`Stamp::is_plausible`]). Adopting
//! it would let that one copy outrank everything copied anywhere until the
//! wall clocks caught up.

use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

/// How far ahead of local wall time a peer's stamp may be.
const MAX_DRIFT: Duration = Duration::from_mins(5);

/// When a copy was made and where: the total order every machine agrees
/// on. Two machines can stamp the same millisecond; the origin breaks the
/// tie.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Stamp {
    /// Milliseconds since the unix epoch.
    pub millis: u64,
    pub origin: EndpointId,
}

impl Stamp {
    /// Stamps a copy made at `origin`, after `last`.
    pub fn next(last: Option<Stamp>, origin: EndpointId) -> Self {
        let after = last.map_or(0, |last| last.millis + 1);

        Stamp {
            millis: wall_millis().max(after),
            origin,
        }
    }

    /// Whether a peer's stamp is close enough to our wall clock to be
    /// believed.
    pub fn is_plausible(self) -> bool {
        self.millis <= wall_millis().saturating_add(millis_of(MAX_DRIFT))
    }

    /// The wall-clock instant this stamp claims to be at.
    pub fn time(self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.millis)
    }
}

impl fmt::Display for Stamp {
    /// A name unique per copy, safe as a file name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let origin = self.origin.to_string();
        write!(f, "{}-{}", self.millis, &origin[..8])
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
    fn stamps_never_repeat_or_go_backwards() {
        let origin = iroh::SecretKey::generate().public();
        let ahead = Stamp {
            millis: wall_millis() + 60_000,
            origin,
        };
        let mut last = Some(ahead);
        for _ in 0..1000 {
            let next = Stamp::next(last, origin);
            assert!(Some(next) > last);
            last = Some(next);
        }
    }
}
