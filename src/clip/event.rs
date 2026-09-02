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
    pub selection: Selection,
    /// Whether this is a password or the like: never written to disk,
    /// never shown, and given a lifetime by default.
    pub secret: bool,
    /// Seconds after which every machine drops it and, if it is still on
    /// the clipboard, empties the clipboard. `None` lets it live until the
    /// history limits push it out.
    pub ttl: Option<u32>,
}

/// A selection, in every type it is carried under.
///
/// An application offers its selection under several types at once and the
/// one that gets pasted is the pasting application's choice, not ours: a
/// browser wants the HTML, a terminal wants the text. Carrying them all is
/// what lets a machine that did not do the copying answer that choice the
/// way the source machine would have.
///
/// The type that names the entry is a field of its own rather than the
/// head of a list, so it is always there: a selection with no
/// representation is not a selection, and postcard refuses one on the way
/// in rather than leaving every reader to wonder.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Selection {
    /// What the entry *is*, and what it is named by in the history.
    pub primary: Rep,
    /// The other types the same selection was offered under, best first.
    pub alternates: Vec<Rep>,
}

/// One representation of a selection.
///
/// Mime is the vocabulary of the mesh, on every platform. A backend for a
/// system that names its own types differently, macOS and its uniform type
/// identifiers for instance, translates at its own edge rather than
/// putting a second vocabulary on the wire: two machines naming the same
/// bytes differently would each have to understand both, forever.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rep {
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl Selection {
    /// Takes the representations of a selection, best first, or nothing
    /// when there is none to name it by.
    pub fn new(reps: Vec<Rep>) -> Option<Self> {
        let mut reps = reps.into_iter();

        Some(Selection {
            primary: reps.next()?,
            alternates: reps.collect(),
        })
    }

    /// A selection carried under one type only.
    pub fn of(rep: Rep) -> Self {
        Selection {
            primary: rep,
            alternates: Vec::new(),
        }
    }

    /// The type that describes the selection.
    pub fn mime(&self) -> &str {
        &self.primary.mime
    }

    /// Every representation, best first.
    pub fn reps(&self) -> impl Iterator<Item = &Rep> {
        std::iter::once(&self.primary).chain(&self.alternates)
    }

    /// The same, giving them up.
    pub fn into_reps(self) -> impl Iterator<Item = Rep> {
        std::iter::once(self.primary).chain(self.alternates)
    }

    /// What the selection weighs: every representation together.
    pub fn size(&self) -> usize {
        self.reps().map(|rep| rep.bytes.len()).sum()
    }

    /// The representation offered under `mime`.
    pub fn rep(&self, mime: &str) -> Option<&Rep> {
        self.reps().find(|rep| rep.mime.eq_ignore_ascii_case(mime))
    }
}

impl Rep {
    pub fn new(mime: impl Into<String>, bytes: Vec<u8>) -> Self {
        Rep {
            mime: mime.into(),
            bytes,
        }
    }
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
            selection: Selection::new(vec![
                Rep::new("text/plain", b"hello".to_vec()),
                Rep::new("text/html", b"<b>hello</b>".to_vec()),
            ])
            .unwrap(),
            secret: true,
            ttl: Some(90),
        });

        let decoded = Event::decode(&event.encode()).unwrap();
        let Event::Copy(copy) = decoded else {
            panic!("expected a copy");
        };
        assert_eq!(copy.selection.mime(), "text/plain");
        assert_eq!(
            copy.selection.rep("TEXT/HTML").unwrap().bytes,
            b"<b>hello</b>",
        );
        assert_eq!(copy.selection.size(), 17);
        assert!(copy.secret);
        assert_eq!(copy.ttl, Some(90));
    }

    /// What the split buys: "a selection carries at least one
    /// representation" is a fact rather than a comment, here and on the
    /// wire alike, so nothing downstream has to answer for an entry that
    /// carries none.
    #[test]
    fn a_selection_with_no_representation_cannot_be_built() {
        assert!(Selection::new(Vec::new()).is_none());
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(Event::decode(&[0xff, 0xff, 0xff]).is_err());
    }
}
