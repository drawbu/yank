//! What a clipboard log entry says.
//!
//! These are the payloads [`crate::log`] carries around. Everything the
//! clipboard does is one of them, including the destructive operations:
//! emptying the clipboard and dropping history are events like any other,
//! so they reach a machine that was offline when they happened instead of
//! being forgotten the moment the network was down.

use color_eyre::eyre::{Result, WrapErr as _};
use serde::{Deserialize, Serialize};

use crate::log::{EntryId, Payload, payload};

/// One clipboard event.
///
/// Postcard encodes variants by position: existing ones keep their
/// position and meaning, new ones are appended. A machine that meets an
/// event it cannot decode ignores it rather than refusing the entry, so a
/// mesh running two versions still works for what both understand.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Event {
    /// Something was copied.
    Copy(Copy),
    /// Empty the clipboard everywhere, leaving the history alone.
    Clear,
    /// Drop one entry from every machine.
    Forget(EntryId),
    /// Drop every entry written before this one. What it covers is its
    /// own place in the log, so a machine that has not caught up cannot
    /// issue a purge that covers nothing.
    Purge,
}

/// A copied selection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Copy {
    /// The mime type `bytes` is in.
    pub mime: String,
    pub bytes: Vec<u8>,
    /// Whether this is a password or the like: never written to disk,
    /// never shown, and given a lifetime by default.
    pub secret: bool,
    /// Seconds after which every machine drops it and, if it is still on
    /// the clipboard, empties the clipboard. `None` lets it live until the
    /// history limits push it out.
    pub ttl: Option<u32>,
}

impl Event {
    /// Encodes the event as a log payload.
    pub fn encode(&self) -> Payload {
        payload(postcard::to_stdvec(self).expect("a clipboard event must serialize"))
    }

    /// Decodes a log payload written by another machine, or read back from
    /// disk.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        postcard::from_bytes(bytes).wrap_err("cannot decode the clipboard event")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip() {
        let event = Event::Copy(Copy {
            mime: "text/plain".to_owned(),
            bytes: b"hello".to_vec(),
            secret: true,
            ttl: Some(90),
        });

        let decoded = Event::decode(&event.encode()).unwrap();
        let Event::Copy(copy) = decoded else {
            panic!("expected a copy");
        };
        assert_eq!(copy.bytes, b"hello");
        assert!(copy.secret);
        assert_eq!(copy.ttl, Some(90));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(Event::decode(&[0xff, 0xff, 0xff]).is_err());
    }
}
