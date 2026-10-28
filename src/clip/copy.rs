//! What the clipboard holds, as machines exchange it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize as _;

use super::{MAX_SIZE, Stamp, mime};
use crate::files::{self, FileRef};

/// How much of a copy [`Copy::preview`] keeps.
const PREVIEW_CHARS: usize = 72;

/// What a file adds to a copy on the wire besides its path: its hash, its
/// size and the length prefixes.
const FILE_OVERHEAD: usize = 48;

/// Cap on a mime type's length.
const MAX_MIME_LEN: usize = 255;

/// The clipboard of the whole mesh: the latest copy, or nothing once it
/// was cleared or its lifetime ran out. Whichever has the newest stamp
/// wins everywhere.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Clip {
    pub stamp: Stamp,
    pub copy: Option<Copy>,
}

/// A copied selection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Copy {
    pub selection: Selection,
    /// Whether this is a password or the like: never written to disk,
    /// never shown, and given a lifetime by default.
    pub secret: bool,
    /// Seconds after which every machine empties its clipboard of it.
    pub ttl: Option<u32>,
    /// The files this selection names, empty when it names none.
    ///
    /// A file copy is a copy of paths (see [`crate::files`]), so this is
    /// what the paths in the selection mean: the contents, by hash, for a
    /// machine that does not have that filesystem to resolve them against.
    pub files: Vec<FileRef>,
}

/// A selection, in every type it is carried under.
///
/// An application offers its selection under several types at once and the
/// one that gets pasted is the pasting application's choice, not ours: a
/// browser wants the HTML, a terminal wants the text. Carrying them all is
/// what lets a machine that did not do the copying answer that choice the
/// way the source machine would have.
///
/// The type that names the selection is a field of its own rather than the
/// head of a list, so it is always there: a selection with no
/// representation is not a selection, and postcard refuses one on the way
/// in rather than leaving every reader to wonder.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Selection {
    /// What the selection *is*.
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

    /// A selection naming files where they are on this machine.
    ///
    /// Three types, because pasting a file means three things: a file
    /// manager reads the URIs, GNOME's reads what to do with them first,
    /// and a terminal reads the paths as text. Built from the paths rather
    /// than carried across, since only the machine holding the files knows
    /// where they are.
    pub fn of_files(paths: &[PathBuf]) -> Self {
        Selection {
            primary: Rep::new(mime::PLAIN_TEXT, files::as_text(paths)),
            alternates: vec![
                Rep::new(mime::URI_LIST, files::uri_list(paths)),
                Rep::new(mime::GNOME_COPIED_FILES, files::gnome_copied_files(paths)),
            ],
        }
    }

    /// The local paths this selection names, empty when it names none.
    ///
    /// The other half of [`Self::of_files`]: what a file manager puts on
    /// the clipboard is read back the same way it is written.
    pub fn file_paths(&self) -> Vec<PathBuf> {
        self.rep(mime::URI_LIST)
            .map(|rep| files::paths(&rep.bytes))
            .unwrap_or_default()
    }

    /// Every representation, best first.
    pub fn reps(&self) -> impl Iterator<Item = &Rep> {
        std::iter::once(&self.primary).chain(&self.alternates)
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

impl Copy {
    /// Whether the selection names files rather than carrying bytes.
    pub fn is_files(&self) -> bool {
        !self.files.is_empty()
    }

    /// What the copy weighs on the wire: every representation, and the
    /// manifest of the files it names.
    pub fn size(&self) -> usize {
        let manifest: usize = self
            .files
            .iter()
            .map(|file| file.path.len() + FILE_OVERHEAD)
            .sum();

        self.selection.size() + manifest
    }

    /// Refuses a copy no machine should hold: too large for the wire, with
    /// types a compositor would choke on, an unsafe manifest, or files on
    /// a secret, which would write it to disk.
    pub fn check(&self) -> eyre::Result<()> {
        eyre::ensure!(self.size() <= MAX_SIZE as usize, "the copy is too large");
        eyre::ensure!(
            self.selection.reps().count() <= mime::MAX_REPS
                && self.selection.reps().all(|rep| {
                    !rep.mime.is_empty()
                        && rep.mime.len() <= MAX_MIME_LEN
                        && rep.mime.chars().all(|c| c.is_ascii_graphic() || c == ' ')
                }),
            "the copy carries unusable types",
        );
        eyre::ensure!(
            self.files.len() <= files::MAX_FILES,
            "the copy names too many files"
        );
        eyre::ensure!(
            !(self.secret && self.is_files()),
            "a secret cannot name files"
        );

        files::validate(&self.files)
    }

    /// One line describing the copy, safe to print.
    pub fn preview(&self) -> String {
        let Rep { mime, bytes } = &self.selection.primary;
        if self.secret {
            return "<secret>".to_owned();
        }
        if !mime::is_text(mime) {
            return format!("<{mime}, {}>", bytesize::ByteSize::b(bytes.len() as u64));
        }

        let text = String::from_utf8_lossy(bytes);
        let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
        let mut preview = crate::config::sanitize(lines.next().unwrap_or(""));
        if let Some((cut, _)) = preview.char_indices().nth(PREVIEW_CHARS) {
            preview.truncate(cut);
            preview.push('…');
        }
        if lines.next().is_some() {
            preview.push_str(" ⏎");
        }

        preview
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

impl Drop for Rep {
    /// Wipes the bytes: the clipboard is where a copied password lives.
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_are_one_safe_line() {
        let preview = |mime: &str, bytes: &[u8], secret| {
            Copy {
                selection: Selection::of(Rep::new(mime, bytes.to_vec())),
                secret,
                ttl: None,
                files: Vec::new(),
            }
            .preview()
        };

        assert_eq!(preview("text/plain", b"one\ntwo", false), "one ⏎");
        assert_eq!(preview("text/plain", b"a\x1b[2Kb", false), "a?[2Kb");
        assert_eq!(
            preview("image/png", &[0; 2048], false),
            "<image/png, 2.0 KiB>"
        );
        assert_eq!(preview("text/plain", b"secret", true), "<secret>");
    }
}
