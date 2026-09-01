//! Hygiene for strings that reach the terminal.
//!
//! Machine names and error messages arrive from other machines and end up
//! in `yank status` output, so everything crossing that boundary is bounded
//! in length and stripped of characters that could hide or reorder what the
//! user sees.

use color_eyre::eyre::{Result, ensure};

/// Maximum length of a machine name, in bytes.
pub const MAX_NAME_LEN: usize = 64;

/// Maximum length kept by [`sanitize_bounded`].
const MAX_SANITIZED_LEN: usize = 256;

/// Checks that a name is usable; `kind` names it in the error. Also applied
/// to names arriving from remote machines.
pub fn validate_name(kind: &str, name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "{kind} name cannot be empty");
    ensure!(
        name.len() <= MAX_NAME_LEN,
        "{kind} name is longer than {MAX_NAME_LEN} bytes",
    );
    ensure!(
        !name.chars().any(is_confusable),
        "{kind} name contains control or invisible characters",
    );

    Ok(())
}

/// Makes a string safe to print as one line: characters that could hide or
/// reorder terminal output become `?`, so tampering stays visible.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if is_confusable(c) { '?' } else { c })
        .collect()
}

/// [`sanitize`] plus a length bound with a `…` marker, for strings whose
/// size is not ours to trust.
pub fn sanitize_bounded(text: &str) -> String {
    let mut clean = sanitize(text);
    if let Some((idx, _)) = clean.char_indices().nth(MAX_SANITIZED_LEN) {
        clean.truncate(idx);
        clean.push('…');
    }
    clean
}

/// Whether a character can hide or reorder text in terminal output:
/// controls (`is_control` covers only `Cc`), zero-width and bidi
/// formatting.
fn is_confusable(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_masks_escape_sequences() {
        assert_eq!(sanitize("plain text"), "plain text");
        assert_eq!(sanitize("a\x1b[2Kb\r\nc"), "a?[2Kb??c");
        assert_eq!(sanitize("admin\u{200B}istrator"), "admin?istrator");
    }

    #[test]
    fn sanitize_bounded_marks_the_cut() {
        let long = "x".repeat(MAX_SANITIZED_LEN + 10);
        let cut = sanitize_bounded(&long);
        assert_eq!(cut.chars().count(), MAX_SANITIZED_LEN + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn names_are_validated() {
        assert!(validate_name("machine", "laptop").is_ok());
        assert!(validate_name("machine", "").is_err());
        assert!(validate_name("machine", "a\u{202E}b").is_err());
        assert!(validate_name("machine", &"x".repeat(MAX_NAME_LEN + 1)).is_err());
    }
}
