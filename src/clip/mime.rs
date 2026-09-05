//! Mime selection policy.
//!
//! A clipboard selection is offered under several mime types at once, and
//! which one gets pasted is the pasting application's choice: a browser
//! asks for the HTML, a terminal asks for the text. yank carries them all
//! so a machine that did not do the copying can answer that choice too.
//! Picking what to carry, and what to announce, is a policy, kept here so
//! capturing and re-offering agree on it.

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

/// The type text is best carried as, and what `yank copy` gives what it
/// reads from the command line.
pub const PLAIN_TEXT: &str = "text/plain;charset=utf-8";

/// How many representations of one selection to carry.
///
/// A handful is all an application offers that is worth having; past that
/// it is listing conversions of the same thing. The cap is what stops one
/// selection from costing a pipe, a thread and a Wayland request per type
/// an application feels like advertising, and there is no upper bound on
/// how many that is.
const MAX_REPS: usize = 8;

/// The type a file reference is carried as: `file://` URIs, one per line.
pub const URI_LIST: &str = "text/uri-list";

/// What GNOME's file managers paste from: what to do with the files, then
/// the URIs.
pub const GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";

/// Text types, best first. The first is what wl-clipboard and every
/// Wayland toolkit prefer; the rest are the X11 names that survive in
/// applications ported from it.
const TEXT: &[&str] = &[PLAIN_TEXT, "text/plain", "UTF8_STRING", "STRING", "TEXT"];

/// Types whose contents name something on the machine that copied it: a
/// file, and what a file manager means to do with it.
///
/// Copying a file puts no file on the clipboard, only its path, and the
/// pasting application is what reads it. That path belongs to one
/// machine. Anywhere else it is missing, or worse, is a different file, so
/// these are only ever offered back where they came from.
const LOCAL: &[&str] = &[
    URI_LIST,
    GNOME_COPIED_FILES,
    "x-special/nautilus-clipboard",
    "application/x-kde-cutselection",
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

/// Picks the types to capture out of what a selection offers, best first.
///
/// The first is what the entry is: text when there is text, because text
/// is what a shared clipboard is mostly for and what every machine can
/// serve back; failing that, the first offered type that carries content
/// at all. The rest is everything else offered, so a pasting application
/// on another machine has the choice the source gave. Text aliases are not
/// among them: [`aliases`] serves those from the one text representation.
///
/// Nothing is captured when the type that would come first is one the user
/// told yank to ignore. `ignore-mime` is about a selection, not about one
/// representation of it: dropping only that one would share the same
/// selection under its next type.
///
pub fn select<'a>(offered: &'a [String], ignore: &[String]) -> Vec<&'a str> {
    let ignored = |mime: &str| ignore.iter().any(|name| name == mime);

    let Some(primary) = choose(offered) else {
        return Vec::new();
    };
    if ignored(primary) {
        return Vec::new();
    }

    let mut chosen = vec![primary];
    for mime in offered.iter().map(String::as_str) {
        if chosen.len() >= MAX_REPS {
            break;
        }
        if !is_content(mime) || is_text_alias(mime) || ignored(mime) {
            continue;
        }
        if chosen.iter().any(|taken| taken.eq_ignore_ascii_case(mime)) {
            continue;
        }
        chosen.push(mime);
    }

    chosen
}

/// The type that describes a selection: the one an entry is named by.
fn choose(offered: &[String]) -> Option<&str> {
    best_text(offered).or_else(|| {
        offered
            .iter()
            .map(String::as_str)
            .find(|mime| is_content(mime))
    })
}

/// The best of the text types a selection offers.
fn best_text(offered: &[String]) -> Option<&str> {
    TEXT.iter().find_map(|wanted| {
        offered
            .iter()
            .find(|mime| mime.eq_ignore_ascii_case(wanted))
            .map(String::as_str)
    })
}

/// The types to announce for a payload captured as `mime`.
///
/// Text goes out under every name for text, the way `wl-copy` does it, so
/// an application asking for `UTF8_STRING` gets the same bytes as one
/// asking for `text/plain`. Anything else is offered as itself, a file
/// reference included: an application asking for `text/plain` wants the
/// path, not the URI list around it.
pub fn aliases(mime: &str) -> Vec<String> {
    if is_local(mime) || !is_text(mime) {
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
    is_text_alias(mime)
        || mime.starts_with("text/")
        || mime == "application/json"
        || mime == "application/xml"
}

/// Whether a type names a file rather than carrying one, and is therefore
/// only true on the machine that copied it.
pub fn is_local(mime: &str) -> bool {
    LOCAL.iter().any(|local| local.eq_ignore_ascii_case(mime))
}

/// Whether a type is one of the interchangeable names for plain text.
fn is_text_alias(mime: &str) -> bool {
    TEXT.iter().any(|text| text.eq_ignore_ascii_case(mime))
}

/// Whether a type carries content at all, as opposed to being selection
/// bookkeeping or one of the bare X11 target names, which only [`TEXT`]
/// knows what to do with.
fn is_content(mime: &str) -> bool {
    !CHATTER.contains(&mime) && mime.contains('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered(mimes: &[&str]) -> Vec<String> {
        mimes.iter().map(|mime| (*mime).to_owned()).collect()
    }

    fn select_all(offered: &[String]) -> Vec<&str> {
        select(offered, &[])
    }

    #[test]
    fn text_wins_over_the_rest() {
        let mimes = offered(&["image/png", "text/plain", "text/plain;charset=utf-8"]);
        assert_eq!(select_all(&mimes)[0], "text/plain;charset=utf-8");
    }

    #[test]
    fn chatter_is_never_content() {
        assert!(select_all(&offered(&["TARGETS", "TIMESTAMP"])).is_empty());
        assert_eq!(
            select_all(&offered(&["TARGETS", "image/png"])),
            vec!["image/png"],
        );
    }

    #[test]
    fn our_own_marker_is_not_content_either() {
        assert!(select_all(&offered(&[MARKER])).is_empty());
    }

    /// What a browser offers: the same selection formatted and plain. Both
    /// are carried, and the plain one is not carried five times over.
    #[test]
    fn every_representation_is_taken_once() {
        let mimes = offered(&[
            "text/html",
            "text/plain;charset=utf-8",
            "text/plain",
            "UTF8_STRING",
            "STRING",
        ]);

        assert_eq!(
            select_all(&mimes),
            vec!["text/plain;charset=utf-8", "text/html"],
        );
    }

    /// An image, with the text an application offers beside it. Text is
    /// what the entry is named by, as always, and the image is carried
    /// with it rather than instead of it.
    #[test]
    fn an_image_is_carried_beside_the_text() {
        let mimes = offered(&["image/png", "image/bmp", "text/plain"]);

        assert_eq!(
            select_all(&mimes),
            vec!["text/plain", "image/png", "image/bmp"],
        );
    }

    #[test]
    fn a_selection_costs_a_bounded_number_of_transfers() {
        let many: Vec<String> = (0..64)
            .map(|index| format!("application/x-{index}"))
            .collect();

        assert_eq!(select_all(&many).len(), MAX_REPS);
    }

    #[test]
    fn an_ignored_type_is_dropped_and_an_ignored_first_one_drops_the_selection() {
        let mimes = offered(&["text/plain", "text/html", "image/png"]);

        assert_eq!(
            select(&mimes, &["text/html".to_owned()]),
            vec!["text/plain", "image/png"],
        );
        assert!(select(&mimes, &["text/plain".to_owned()]).is_empty());
    }

    #[test]
    fn text_is_offered_under_every_name() {
        let text = aliases("text/plain;charset=utf-8");
        assert_eq!(text[0], "text/plain;charset=utf-8");
        assert!(text.contains(&"UTF8_STRING".to_owned()));
        assert_eq!(text.len(), TEXT.len());

        assert_eq!(aliases("image/png"), vec!["image/png".to_owned()]);
        assert_eq!(aliases("text/uri-list"), vec!["text/uri-list".to_owned()]);
    }

    #[test]
    fn a_file_reference_is_local_and_its_contents_are_not() {
        assert!(is_local("text/uri-list"));
        assert!(is_local("x-special/gnome-copied-files"));
        assert!(!is_local("text/plain"));
        assert!(!is_local("image/png"));
    }
}
