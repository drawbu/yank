//! Which mime type to take, and which ones to offer back.
//!
//! A clipboard selection is offered under several mime types at once, and
//! yank carries exactly one representation of it. Picking that one is a
//! policy, kept here so capturing and re-offering agree on it.

/// The type yank adds to every selection it owns.
///
/// It is how the daemon recognizes its own selection: applying an entry
/// makes the compositor announce a new selection, and without a marker
/// that announcement is indistinguishable from the user copying something,
/// which would append a duplicate entry on every machine, forever.
pub const MARKER: &str = "application/x-yank";

/// What password managers offer alongside a password so clipboard managers
/// leave it alone. yank does not drop such an entry, since the point is to
/// paste it on another machine, but treats it as secret: memory only, and
/// gone after the configured lifetime.
pub const SECRET_HINT: &str = "x-kde-passwordManagerHint";

/// Text types, best first. The first is what wl-clipboard and every
/// Wayland toolkit prefer; the rest are the X11 names that survive in
/// applications ported from it.
const TEXT: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// Selection targets that are protocol chatter rather than content.
const CHATTER: &[&str] = &[
    "TARGETS",
    "TIMESTAMP",
    "MULTIPLE",
    "SAVE_TARGETS",
    "DELETE",
    "INSERT_SELECTION",
    "INSERT_PROPERTY",
    MARKER,
    SECRET_HINT,
];

/// Picks the type to capture out of what a selection offers.
///
/// Text wins over anything else, because text is what a shared clipboard
/// is mostly for and because it is the one representation every machine
/// can serve back. Failing that, the first offered type that carries
/// content at all.
pub fn choose(offered: &[String]) -> Option<&str> {
    for wanted in TEXT {
        if let Some(found) = offered
            .iter()
            .find(|mime| mime.eq_ignore_ascii_case(wanted))
        {
            return Some(found);
        }
    }

    offered
        .iter()
        .map(String::as_str)
        .find(|mime| !is_chatter(mime) && mime.contains('/'))
}

/// The types to announce for a payload captured as `mime`.
///
/// Text goes out under every name for text, the way `wl-copy` does it, so
/// an application asking for `UTF8_STRING` gets the same bytes as one
/// asking for `text/plain`. Anything else is offered as itself.
pub fn aliases(mime: &str) -> Vec<String> {
    if !is_text(mime) {
        return vec![mime.to_owned()];
    }

    let mut aliases = vec![mime.to_owned()];
    aliases.extend(
        TEXT.iter()
            .filter(|alias| !alias.eq_ignore_ascii_case(mime))
            .map(|alias| (*alias).to_owned()),
    );

    aliases
}

/// Whether a payload in this type is text, and can therefore be shown as a
/// preview and offered under the text aliases.
pub fn is_text(mime: &str) -> bool {
    TEXT.iter().any(|text| text.eq_ignore_ascii_case(mime))
        || mime.starts_with("text/")
        || mime == "application/json"
        || mime == "application/xml"
}

/// Whether a type is selection bookkeeping rather than content.
fn is_chatter(mime: &str) -> bool {
    CHATTER.contains(&mime)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered(mimes: &[&str]) -> Vec<String> {
        mimes.iter().map(|mime| (*mime).to_owned()).collect()
    }

    #[test]
    fn text_wins_over_the_rest() {
        let mimes = offered(&["image/png", "text/plain", "text/plain;charset=utf-8"]);
        assert_eq!(choose(&mimes), Some("text/plain;charset=utf-8"));
    }

    #[test]
    fn chatter_is_never_content() {
        assert_eq!(choose(&offered(&["TARGETS", "TIMESTAMP"])), None);
        assert_eq!(
            choose(&offered(&["TARGETS", "image/png"])),
            Some("image/png"),
        );
    }

    #[test]
    fn our_own_marker_is_not_content_either() {
        assert_eq!(choose(&offered(&[MARKER])), None);
    }

    #[test]
    fn text_is_offered_under_every_name() {
        let text = aliases("text/plain;charset=utf-8");
        assert_eq!(text[0], "text/plain;charset=utf-8");
        assert!(text.contains(&"UTF8_STRING".to_owned()));
        assert_eq!(text.len(), TEXT.len());

        assert_eq!(aliases("image/png"), vec!["image/png".to_owned()]);
    }
}
