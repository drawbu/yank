//! Files on the clipboard.
//!
//! Copying a file puts no file on the clipboard. It puts a path, under
//! `text/uri-list`, and the pasting application is what opens it. That
//! works because both ends share a filesystem; across machines they do
//! not, so a path copied here is at best missing there and at worst a
//! different file. yank turns the reference into content:
//!
//! ```text
//!   copy   paths ──► spool, one file per hash ──► manifest in the entry
//!   paste  manifest ──► fetch the hashes from a peer ──► tree of links
//!                                                    └─► text/uri-list
//! ```
//!
//! The manifest is small and rides the log like any other entry, so every
//! machine knows what an entry names as soon as it hears of it. The bytes
//! do not: they are named by hash, pulled from whoever has them
//! ([`crate::net::proto::ContentRequest`]) and never enter the log.
//!
//! [`Store`] owns both halves of the spool: content by hash, and the tree
//! of hard links an entry is laid out as, which is what a `file://` URI on
//! a machine that did not do the copying points at.

mod store;

use std::{
    ffi::OsString,
    fmt,
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, ensure};
use data_encoding::HEXLOWER;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode, percent_encode};
use serde::{Deserialize, Serialize};

pub use self::store::{Source, Store, walk};

/// What a `file:` URI may carry unescaped: the unreserved characters of
/// RFC 3986, and the separator between path components. Everything else,
/// every non-ASCII byte included, is escaped, which is what makes a path
/// that is not UTF-8 survive the trip.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

/// The name of some content: the blake3 hash of the bytes.
///
/// Content-addressed rather than named, so a file already spooled under
/// another entry is never fetched twice, and so a transfer that ends with
/// the wrong bytes is caught by the name it was asked for.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hash([u8; 32]);

/// One file a selection names.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRef {
    /// Where the file sits under the entry's root: `photos/a.png`. Always
    /// relative, and never climbing out of it (see [`Self::is_safe`]); it
    /// is a path a peer wrote, and it is used to create files.
    pub path: String,
    pub size: u64,
    pub hash: Hash,
}

impl Hash {
    /// The hash of a whole buffer.
    pub fn of(bytes: &[u8]) -> Self {
        Hash(*blake3::hash(bytes).as_bytes())
    }

    /// The file name the content is spooled under.
    pub fn file_name(&self) -> String {
        HEXLOWER.encode(&self.0)
    }

    /// The hash a spooled file's name says it is, if it says one at all.
    pub fn parse(name: &str) -> Option<Self> {
        let mut hash = [0u8; 32];
        if name.len() != 2 * hash.len() {
            return None;
        }
        HEXLOWER.decode_mut(name.as_bytes(), &mut hash).ok()?;

        Some(Hash(hash))
    }
}

impl From<blake3::Hash> for Hash {
    fn from(hash: blake3::Hash) -> Self {
        Hash(*hash.as_bytes())
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", &self.file_name()[..16])
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl FileRef {
    /// Whether the path is one this machine may create.
    ///
    /// The manifest comes from another machine, and the path in it is used
    /// to make a file: an absolute path, or one climbing out with `..`,
    /// would write wherever it liked.
    pub fn is_safe(&self) -> bool {
        !self.path.is_empty()
            && !self.path.contains('\0')
            && Path::new(&self.path)
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
    }
}

/// What a selection of files weighs.
pub fn total(files: &[FileRef]) -> Result<u64> {
    files.iter().try_fold(0u64, |total, file| {
        total
            .checked_add(file.size)
            .ok_or_else(|| color_eyre::eyre::eyre!("the file manifest is too large"))
    })
}

/// Checks the manifest before it drives disk or network work.
pub fn validate(files: &[FileRef]) -> Result<()> {
    ensure!(
        files.iter().all(FileRef::is_safe),
        "a file path is not safe"
    );
    ensure!(
        files
            .iter()
            .all(|file| file.path.split('/').all(|part| !part.is_empty())),
        "a file path is not normalized",
    );
    let mut paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
    paths.sort_unstable();
    ensure!(
        paths.windows(2).all(|pair| {
            pair[0] != pair[1]
                && !pair[1]
                    .strip_prefix(pair[0])
                    .is_some_and(|rest| rest.starts_with('/'))
        }),
        "two files would occupy the same path",
    );
    total(files)?;
    Ok(())
}

/// The local paths a `text/uri-list` payload names.
///
/// Anything that is not a `file:` URI is skipped rather than refused: a
/// selection may name a web page and a file at once, and the file half is
/// still worth having.
pub fn paths(uri_list: &[u8]) -> Vec<PathBuf> {
    uri_list
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        // RFC 2483 makes a line starting with `#` a comment.
        .filter(|line| !line.is_empty() && !line.starts_with(b"#"))
        .filter_map(local_path)
        .collect()
}

/// The `text/uri-list` naming paths as they are on this machine.
pub fn uri_list(paths: &[PathBuf]) -> Vec<u8> {
    let mut list = Vec::new();
    for path in paths {
        list.extend_from_slice(b"file://");
        escape(&mut list, path);
        list.extend_from_slice(b"\r\n");
    }

    list
}

/// What GNOME's file managers read: what to do with the files, then the
/// URIs, newline-separated.
///
/// Always `copy`. A `cut` pasted here would move the one copy of the
/// content out of the spool, and the file it was cut from is on another
/// machine, which no paste is going to reach.
pub fn gnome_copied_files(paths: &[PathBuf]) -> Vec<u8> {
    let mut list = b"copy".to_vec();
    for path in paths {
        list.extend_from_slice(b"\nfile://");
        escape(&mut list, path);
    }

    list
}

/// The paths as the text of them: what a terminal or an editor gets out of
/// pasting a copied file.
pub fn as_text(paths: &[PathBuf]) -> Vec<u8> {
    let mut text = Vec::new();
    for path in paths {
        text.extend_from_slice(path.as_os_str().as_bytes());
        text.push(b'\n');
    }

    text
}

/// The path a `file:` URI names, if it names one on this machine.
fn local_path(uri: &[u8]) -> Option<PathBuf> {
    let rest = uri.strip_prefix(b"file:")?;
    // `file:/path`, `file:///path`, and `file://host/path`, which is
    // somebody else's file whatever the host says.
    let path = match rest.strip_prefix(b"//") {
        Some(authority) => {
            let start = authority.iter().position(|byte| *byte == b'/')?;
            if start != 0 {
                return None;
            }
            &authority[start..]
        }
        None => rest,
    };

    Some(PathBuf::from(OsString::from_vec(
        percent_decode(path).collect(),
    )))
}

/// Appends a path to a URI being built, escaped.
fn escape(uri: &mut Vec<u8>, path: &Path) {
    for piece in percent_encode(path.as_os_str().as_bytes(), UNRESERVED) {
        uri.extend_from_slice(piece.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> FileRef {
        FileRef {
            path: path.to_owned(),
            size: 0,
            hash: Hash::of(b""),
        }
    }

    fn under(root: &str, files: &[FileRef]) -> Vec<PathBuf> {
        files
            .iter()
            .map(|file| Path::new(root).join(&file.path))
            .collect()
    }

    #[test]
    fn a_uri_list_round_trips_through_a_root() {
        let files = vec![file("a b.txt"), file("photos/été.png")];
        let list = uri_list(&under("/spool/x", &files));

        assert_eq!(
            String::from_utf8_lossy(&list),
            "file:///spool/x/a%20b.txt\r\nfile:///spool/x/photos/%C3%A9t%C3%A9.png\r\n",
        );
        assert_eq!(
            paths(&list),
            vec![
                PathBuf::from("/spool/x/a b.txt"),
                PathBuf::from("/spool/x/photos/été.png"),
            ],
        );
    }

    #[test]
    fn a_file_manager_is_told_to_copy_and_never_to_move() {
        let list = gnome_copied_files(&under("/spool/x", &[file("a.txt")]));

        assert_eq!(
            String::from_utf8_lossy(&list),
            "copy\nfile:///spool/x/a.txt"
        );
    }

    #[test]
    fn only_local_files_are_taken_out_of_a_uri_list() {
        let list = b"# a comment\r\nhttps://example.com/x\r\nfile:///tmp/one\r\n\
                     file://elsewhere/tmp/two\r\nfile:/tmp/three\r\n";

        assert_eq!(
            paths(list),
            vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/three")],
        );
    }

    #[test]
    fn a_spooled_name_is_read_back_as_the_hash_it_is() {
        let hash = Hash::of(b"hello");
        assert_eq!(Hash::parse(&hash.file_name()), Some(hash));
        assert_eq!(Hash::parse("not-a-hash"), None);
        assert_eq!(Hash::parse(&format!("{}.part", hash.file_name())), None);
    }

    #[test]
    fn a_path_from_a_peer_may_not_climb_out_of_the_tree() {
        assert!(file("photos/a.png").is_safe());
        assert!(!file("../../.ssh/authorized_keys").is_safe());
        assert!(!file("/etc/passwd").is_safe());
        assert!(!file("").is_safe());
    }

    /// A path from another application, half-escaped or not escaped at
    /// all, is still a path worth having.
    #[test]
    fn half_an_escape_is_not_a_lost_path() {
        assert_eq!(
            paths(b"file:///tmp/100%\r\nfile:///tmp/%zz\r\nfile:///tmp/%41\r\n"),
            vec![
                PathBuf::from("/tmp/100%"),
                PathBuf::from("/tmp/%zz"),
                PathBuf::from("/tmp/A"),
            ],
        );
    }
}
