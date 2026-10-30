//! The clipboard as a state machine.
//!
//! The mesh shares one [`Clip`], and the one with the newest [`Stamp`]
//! wins everywhere. A copy here, a clip from a peer and a clear are all
//! the same move: a newer stamp replaces an older one, whatever order they
//! arrive in, so every machine settles on the same clip without
//! reconciling anything.
//!
//! A clip reaches the local clipboard once, when it wins, and nothing puts
//! it back afterwards. An application that takes the selection and exits
//! leaves this clipboard empty, the way any Wayland clipboard is left
//! empty. Re-asserting it would mean racing the application that took the
//! selection, and what the race costs is whatever the user copied a moment
//! ago.
//!
//! No I/O happens here. Mutations return [`Effect`]s and the caller
//! performs them, which is what lets the whole thing be tested without a
//! compositor, a disk or a peer.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use super::{
    Clip, Copy, Rep, Selection, Stamp,
    backend::{Captured, Command, Serve},
    mime,
};
use crate::{
    config::Settings,
    files::{FileRef, Hash},
};

/// What a mutation asks the caller to do.
#[derive(Debug)]
pub enum Effect {
    /// Write [`Clipboard::saved`] to disk.
    Save,
    /// Put a selection on the local clipboard, or empty it.
    Backend(Command),
    /// Bring the files of the clip with this stamp onto this machine.
    /// Until they are here it is on the clipboard as text.
    Fetch(Stamp),
}

/// Whether a direction of the clipboard is running.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Switch {
    #[default]
    On,
    /// Off, until the given time or until `yank resume`.
    Paused { until: Option<SystemTime> },
}

/// Which directions of the clipboard are running.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pause {
    /// Sharing what is copied here.
    pub capture: Switch,
    /// Putting what others copied on this clipboard.
    pub apply: Switch,
}

/// What survives a restart. A secret is saved as a clear: its stamp, so a
/// restart cannot bring an older clip back, and none of its bytes.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Saved {
    pub clip: Option<Arc<Clip>>,
    pub pause: Pause,
}

/// The clipboard of this machine.
#[derive(Debug)]
pub struct Clipboard {
    origin: EndpointId,
    settings: Arc<Settings>,
    /// `None` until anything was copied anywhere. Shared, so serving it to
    /// a peer or saving it costs no copy.
    clip: Option<Arc<Clip>>,
    /// When the copy disappears, if it has a lifetime.
    expires: Option<SystemTime>,
    /// The clip we last put on the local clipboard, whether or not it is
    /// still there, so we neither put it on again nor mistake it for
    /// something the user copied.
    applied: Option<Stamp>,
    /// What its first representation was, which for a peer's files is not
    /// the clip's own: an application re-announcing it is not a new copy.
    served: Option<Hash>,
    /// Where the clip's files are laid out here, once they are. Until then
    /// its paths are another machine's, and are offered as text.
    tree: Option<PathBuf>,
    pause: Pause,
}

impl Switch {
    /// Whether the direction is running at `now`, a paused-until having
    /// elapsed counting as running.
    pub fn is_on(self, now: SystemTime) -> bool {
        match self {
            Switch::On => true,
            Switch::Paused { until } => until.is_some_and(|until| now >= until),
        }
    }
}

impl Clipboard {
    /// Resumes from what was saved. Nothing reaches the clipboard for it:
    /// what the session holds is the compositor's to report, and it may be
    /// something copied while the daemon was down.
    pub fn new(origin: EndpointId, settings: Arc<Settings>, saved: Saved) -> Self {
        let mut board = Clipboard {
            origin,
            settings,
            clip: None,
            expires: None,
            applied: None,
            served: None,
            tree: None,
            pause: saved.pause,
        };
        if let Some(clip) = saved.clip {
            board.set(clip);
            let _ = board.expire(SystemTime::now());
            board.applied = board.current().map(|(stamp, _)| stamp);
        }

        board
    }

    /// What to write to disk.
    pub fn saved(&self) -> Saved {
        let clip = self.clip.as_ref().map(|clip| match &clip.copy {
            Some(copy) if copy.secret => Arc::new(Clip {
                stamp: clip.stamp,
                copy: None,
            }),
            _ => clip.clone(),
        });

        Saved {
            clip,
            pause: self.pause,
        }
    }

    pub fn clip(&self) -> Option<&Arc<Clip>> {
        self.clip.as_ref()
    }

    /// The stamp of the clip, to announce to peers.
    pub fn head(&self) -> Option<Stamp> {
        self.clip.as_ref().map(|clip| clip.stamp)
    }

    /// What the clipboard holds and its stamp, `None` when it is empty.
    pub fn current(&self) -> Option<(Stamp, &Copy)> {
        let clip = self.clip.as_ref()?;

        Some((clip.stamp, clip.copy.as_ref()?))
    }

    pub fn pause(&self) -> Pause {
        self.pause
    }

    /// Whether the clip was made on this machine. The stamp is a peer's
    /// to write, which is why [`Self::accept`] refuses one claiming our
    /// origin: its paths would be offered as files here.
    pub fn is_ours(&self) -> bool {
        self.head().is_some_and(|stamp| stamp.origin == self.origin)
    }

    /// Where the clip's files are laid out here, if they are.
    pub fn tree(&self) -> Option<&Path> {
        self.tree.as_deref()
    }

    /// When the copy disappears, if it has a lifetime.
    pub fn expires_at(&self) -> Option<SystemTime> {
        self.expires
    }

    /// Records something copied on this machine, from `yank copy`.
    ///
    /// Unlike [`Self::captured`] the bytes are not on the clipboard yet,
    /// so this both shares them and puts them there.
    pub fn copy(
        &mut self,
        selection: Selection,
        files: Vec<FileRef>,
        secret: bool,
        ttl: Option<Duration>,
    ) -> eyre::Result<Vec<Effect>> {
        let size = selection.size();
        eyre::ensure!(size > 0, "nothing to copy");
        eyre::ensure!(
            size <= self.settings.max_bytes(),
            "the selection is larger than the {} limit in config.toml",
            self.settings.max_size,
        );
        let copy = Copy {
            selection,
            secret,
            ttl: self.lifetime(secret, ttl),
            files,
        };
        copy.check()?;

        let mut effects = self.replace(Some(copy));
        effects.extend(self.reconcile(SystemTime::now()));

        Ok(effects)
    }

    /// Records something the compositor says was copied.
    ///
    /// The bytes are already on the clipboard, so the clip is marked as
    /// applied: putting them back would take the selection away from the
    /// application that owns it, for nothing.
    pub fn captured(&mut self, captured: Captured, files: Vec<FileRef>) -> Vec<Effect> {
        if !self.settings.capture || !self.pause.capture.is_on(SystemTime::now()) {
            return Vec::new();
        }
        let Some(selection) = self.fit(captured.selection) else {
            tracing::debug!("ignoring a selection over the configured limit");
            return Vec::new();
        };

        // The clipboard already holding the clip is the normal case after
        // a restart, or after an application re-announced what we
        // applied. The first representation is enough to tell: it is the
        // one such an application announces it under.
        if let Some(clip) = &self.clip
            && let Some(copy) = &clip.copy
            && (copy.selection.primary.bytes == selection.primary.bytes
                || self
                    .served
                    .is_some_and(|served| served == Hash::of(&selection.primary.bytes)))
        {
            self.applied = Some(clip.stamp);
            return Vec::new();
        }

        let copy = Copy {
            selection,
            secret: captured.secret,
            ttl: self.lifetime(captured.secret, None),
            files,
        };
        if let Err(err) = copy.check() {
            tracing::debug!("ignoring a selection: {err:#}");
            return Vec::new();
        }
        let effects = self.replace(Some(copy));
        self.applied = self.head();

        effects
    }

    /// Takes a clip from a peer, if it is newer than ours.
    pub fn accept(&mut self, clip: Clip) -> eyre::Result<Vec<Effect>> {
        if Some(clip.stamp) <= self.head() {
            return Ok(Vec::new());
        }
        eyre::ensure!(
            clip.stamp.origin != self.origin,
            "it claims to come from this machine",
        );
        eyre::ensure!(
            clip.stamp.is_plausible(),
            "its clock is too far ahead of ours",
        );
        if let Some(copy) = &clip.copy {
            copy.check()?;
        }

        self.set(Arc::new(clip));
        let mut effects = vec![Effect::Save];
        // Its lifetime may have run out on the way.
        effects.extend(self.expire(SystemTime::now()));

        Ok(effects)
    }

    /// Empties the clipboard on every machine.
    pub fn clear(&mut self) -> Vec<Effect> {
        let mut effects = self.replace(None);
        effects.extend(self.reconcile(SystemTime::now()));

        effects
    }

    /// Pauses or resumes a direction.
    pub fn set_pause(&mut self, capture: Option<Switch>, apply: Option<Switch>) -> Vec<Effect> {
        if let Some(capture) = capture {
            self.pause.capture = capture;
        }
        if let Some(apply) = apply {
            self.pause.apply = apply;
        }

        let mut effects = vec![Effect::Save];
        effects.extend(self.reconcile(SystemTime::now()));

        effects
    }

    /// Catches up with time: drops the copy once its lifetime has run out,
    /// and applies what a pause that has lapsed held back.
    ///
    /// Expiry needs no agreement between machines: they all hold the same
    /// clip, so they all drop it at about the same moment without a word.
    /// It works with the network down, which is the point of a lifetime on
    /// a password. The stamp stays, so no older clip comes back.
    pub fn expire(&mut self, now: SystemTime) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.expires.is_some_and(|deadline| now >= deadline)
            && let Some(clip) = &self.clip
        {
            let stamp = clip.stamp;
            self.set(Arc::new(Clip { stamp, copy: None }));
            effects.push(Effect::Save);
        }
        effects.extend(self.reconcile(now));

        effects
    }

    /// Records that the files of the clip stamped `stamp` are laid out
    /// under `tree`, and offers them as files.
    pub fn materialized(&mut self, stamp: Stamp, tree: PathBuf) -> Vec<Effect> {
        if self.head() != Some(stamp) {
            return Vec::new();
        }
        self.tree = Some(tree);
        // It has to be applied again for the file types to appear.
        self.applied = None;

        self.reconcile(SystemTime::now())
    }

    /// Makes a new clip from this machine the current one.
    fn replace(&mut self, copy: Option<Copy>) -> Vec<Effect> {
        let stamp = Stamp::next(self.head(), self.origin);
        self.set(Arc::new(Clip { stamp, copy }));

        vec![Effect::Save]
    }

    /// Makes `clip` the current one.
    ///
    /// A lifetime counts from the stamp, but never from later than now: a
    /// peer whose clock runs ahead must not keep a password around longer
    /// than it promised.
    fn set(&mut self, clip: Arc<Clip>) {
        let now = SystemTime::now();
        self.expires = clip.copy.as_ref().and_then(|copy| {
            let ttl = Duration::from_secs(u64::from(copy.ttl?));
            Some(clip.stamp.time().min(now) + ttl)
        });
        self.tree = None;
        self.clip = Some(clip);
    }

    /// Brings the local clipboard in line with the clip.
    fn reconcile(&mut self, now: SystemTime) -> Vec<Effect> {
        if !self.settings.apply || !self.pause.apply.is_on(now) {
            return Vec::new();
        }
        let Some(clip) = &self.clip else {
            return Vec::new();
        };

        match &clip.copy {
            Some(copy) if self.applied != Some(clip.stamp) => {
                self.applied = Some(clip.stamp);

                // Asked for once, as the clip reaches the clipboard, and
                // answered by [`Self::materialized`] whenever it lands.
                let mut effects = Vec::new();
                if copy.is_files() && self.settings.files && !self.is_ours() && self.tree.is_none()
                {
                    effects.push(Effect::Fetch(clip.stamp));
                }
                let carried = self.carried(copy);
                self.served = Some(Hash::of(&carried.primary.bytes));
                effects.push(Effect::Backend(Command::Offer(serves(&carried))));

                effects
            }
            None if self.applied.is_some() => {
                self.applied = None;

                vec![Effect::Backend(Command::Clear)]
            }
            _ => Vec::new(),
        }
    }

    /// The representations of the copy that mean anything on this
    /// machine.
    ///
    /// A path is only true where the files are: on the machine that copied
    /// them, and on any machine that has fetched them since. Until then
    /// what is left of it is the text of those paths, which is at least
    /// honest.
    fn carried(&self, copy: &Copy) -> Selection {
        match &self.tree {
            Some(tree) => rehomed(copy, tree),
            None if self.is_ours() => copy.selection.clone(),
            None => stripped(copy),
        }
    }

    /// Drops the alternates that do not fit the size cap, from the back,
    /// since they arrive best first. Nothing fits when the primary alone
    /// is over it: that is a selection too big to share, not one to share
    /// in part.
    fn fit(&self, mut selection: Selection) -> Option<Selection> {
        let cap = self.settings.max_bytes();
        let mut size = selection.size();
        while size > cap {
            size -= selection.alternates.pop()?.bytes.len();
        }

        Some(selection)
    }

    /// The lifetime a copy gets: what was asked for, or the configured
    /// default when it is a secret and nothing was asked.
    fn lifetime(&self, secret: bool, ttl: Option<Duration>) -> Option<u32> {
        let ttl = ttl.or_else(|| secret.then_some(self.settings.secret_ttl))?;

        Some(u32::try_from(ttl.as_secs()).unwrap_or(u32::MAX))
    }
}

/// The selection with its file references taken out: the paths in them
/// are another machine's, and this one has nothing to resolve them
/// against.
///
/// A copy that was nothing but paths keeps them as text, since a path is
/// something a person can read, and `text/uri-list` is the one of those
/// types that is nothing but paths.
fn stripped(copy: &Copy) -> Selection {
    let (paths, kept): (Vec<&Rep>, Vec<&Rep>) = copy
        .selection
        .reps()
        .partition(|rep| mime::is_local(&rep.mime));
    if let Some(selection) = Selection::new(kept.into_iter().cloned().collect()) {
        return selection;
    }

    let bytes = paths
        .iter()
        .find(|rep| rep.mime.eq_ignore_ascii_case(mime::URI_LIST))
        .or(paths.first())
        .expect("a selection carries at least one representation")
        .bytes
        .clone();

    Selection::of(Rep {
        mime: mime::PLAIN_TEXT.to_owned(),
        bytes,
    })
}

/// The selection, naming the copies of its files that are on this
/// machine.
///
/// Every representation that names where the files are is rebuilt rather
/// than carried across, since what it has to say is where they are *here*.
/// What the source offered besides is kept.
fn rehomed(copy: &Copy, tree: &Path) -> Selection {
    let paths: Vec<PathBuf> = copy
        .files
        .iter()
        .map(|file| tree.join(&file.path))
        .collect();

    let mut selection = Selection::of_files(&paths);
    selection.alternates.extend(
        copy.selection
            .reps()
            .filter(|rep| !mime::is_local(&rep.mime) && !mime::is_text(&rep.mime))
            .cloned(),
    );

    selection
}

/// What the compositor is handed for a selection: every representation
/// under its own type, text under all of the names for text (see
/// [`mime::aliases`]).
fn serves(selection: &Selection) -> Vec<Serve> {
    let mut serves: Vec<Serve> = Vec::new();
    for rep in selection.reps() {
        let mimes: Vec<String> = mime::aliases(&rep.mime)
            .into_iter()
            .filter(|mime| !serves.iter().any(|serve| serve.mimes.contains(mime)))
            .collect();
        if mimes.is_empty() {
            continue;
        }
        serves.push(Serve {
            mimes,
            bytes: rep.bytes.clone(),
        });
    }

    serves
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::Hash;

    fn clipboard() -> Clipboard {
        with_settings(Settings::default())
    }

    fn with_settings(settings: Settings) -> Clipboard {
        Clipboard::new(
            iroh::SecretKey::generate().public(),
            Arc::new(settings),
            Saved::default(),
        )
    }

    fn text(text: &str) -> Selection {
        Selection::of(Rep::new("text/plain", text.as_bytes().to_vec()))
    }

    fn selection(reps: Vec<Rep>) -> Selection {
        Selection::new(reps).unwrap()
    }

    fn copy(board: &mut Clipboard, contents: &str) -> Vec<Effect> {
        board.copy(text(contents), Vec::new(), false, None).unwrap()
    }

    fn captured(contents: &str, secret: bool) -> Captured {
        Captured {
            selection: text(contents),
            secret,
        }
    }

    /// What `from` would answer a pull with.
    fn pulled(from: &Clipboard) -> Clip {
        Clip::clone(from.clip().unwrap())
    }

    fn current(board: &Clipboard) -> Option<String> {
        board
            .current()
            .map(|(_, copy)| String::from_utf8_lossy(&copy.selection.primary.bytes).into_owned())
    }

    /// The first representation of everything put on the clipboard.
    fn applied(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Backend(Command::Offer(serves)) => {
                    Some(String::from_utf8_lossy(&serves[0].bytes).into_owned())
                }
                _ => None,
            })
            .collect()
    }

    /// What one mutation put on the clipboard, by type.
    fn served(effects: &[Effect]) -> Vec<(Vec<String>, String)> {
        effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Backend(Command::Offer(serves)) => Some(serves),
                _ => None,
            })
            .expect("nothing was put on the clipboard")
            .iter()
            .map(|serve| {
                (
                    serve.mimes.clone(),
                    String::from_utf8_lossy(&serve.bytes).into_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn what_the_compositor_reports_is_shared_but_not_put_back() {
        let mut board = clipboard();
        let effects = board.captured(captured("typed elsewhere", false), Vec::new());

        assert!(board.head().is_some());
        assert!(applied(&effects).is_empty());
        assert_eq!(current(&board).as_deref(), Some("typed elsewhere"));
    }

    /// What we served for a peer's paths is their text, not the copy's
    /// own first representation: an application announcing it back is not
    /// a new copy.
    #[test]
    fn a_peers_paths_coming_back_are_not_a_new_copy() {
        let mut peer = clipboard();
        let paths = Selection::new(vec![
            Rep::new(
                mime::GNOME_COPIED_FILES,
                b"copy\nfile:///home/x/y.iso".to_vec(),
            ),
            Rep::new(mime::URI_LIST, b"file:///home/x/y.iso".to_vec()),
        ])
        .unwrap();
        peer.copy(paths, Vec::new(), false, None).unwrap();

        let mut board = clipboard();
        let (_, text) = served(&board.accept(pulled(&peer)).unwrap()).remove(0);
        let head = board.head();

        assert!(
            board
                .captured(captured(&text, false), Vec::new())
                .is_empty()
        );
        assert_eq!(board.head(), head);
    }

    #[test]
    fn the_same_content_coming_back_is_not_a_new_copy() {
        let mut board = clipboard();
        copy(&mut board, "hello");
        let head = board.head();

        assert!(
            board
                .captured(captured("hello", false), Vec::new())
                .is_empty()
        );
        assert_eq!(board.head(), head);
    }

    #[test]
    fn the_newest_copy_wins_whatever_order_it_arrives_in() {
        let (mut first, mut second) = (clipboard(), clipboard());
        copy(&mut first, "older");
        std::thread::sleep(Duration::from_millis(2));
        copy(&mut second, "newer");

        let mut board = clipboard();
        assert_eq!(applied(&board.accept(pulled(&second)).unwrap()), ["newer"]);
        assert!(board.accept(pulled(&first)).unwrap().is_empty());
        assert_eq!(current(&board).as_deref(), Some("newer"));

        // The machine that copied the older one adopts the newer too.
        assert_eq!(applied(&first.accept(pulled(&second)).unwrap()), ["newer"]);
    }

    #[test]
    fn clearing_wins_like_a_copy() {
        let mut peer = clipboard();
        copy(&mut peer, "something");
        let mut board = clipboard();
        board.accept(pulled(&peer)).unwrap();

        peer.clear();
        let effects = board.accept(pulled(&peer)).unwrap();

        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Backend(Command::Clear)))
        );
        assert!(board.current().is_none());
    }

    #[test]
    fn a_peer_clock_far_ahead_is_refused() {
        let mut peer = clipboard();
        copy(&mut peer, "from the future");
        let mut clip = pulled(&peer);
        clip.stamp.millis += 24 * 3600 * 1000;

        let mut board = clipboard();
        assert!(board.accept(clip).is_err());
        assert!(board.head().is_none());
    }

    /// A peer writes the stamp. One claiming our origin would have its
    /// paths offered as files here, pointing at whatever it likes.
    #[test]
    fn a_clip_claiming_to_be_ours_is_refused() {
        let mut board = clipboard();
        let mut peer = clipboard();
        copy(&mut peer, "file:///home/me/.ssh/id_ed25519");
        let mut clip = pulled(&peer);
        clip.stamp.origin = board.origin;

        assert!(board.accept(clip).is_err());
    }

    /// A type a compositor cannot announce would take the backend down on
    /// every machine.
    #[test]
    fn a_clip_with_unusable_types_is_refused() {
        let mut peer = clipboard();
        copy(&mut peer, "hello");
        let mut clip = pulled(&peer);
        let copy = clip.copy.as_mut().unwrap();
        copy.selection
            .alternates
            .push(Rep::new("text/html\0", Vec::new()));

        assert!(clipboard().accept(clip).is_err());
    }

    #[test]
    fn a_lifetime_empties_the_clipboard_and_keeps_the_stamp() {
        let mut board = clipboard();
        board
            .copy(
                text("hunter2"),
                Vec::new(),
                false,
                Some(Duration::from_secs(1)),
            )
            .unwrap();
        let head = board.head();

        let effects = board.expire(SystemTime::now() + Duration::from_secs(2));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Backend(Command::Clear)))
        );
        assert!(board.current().is_none());
        assert_eq!(board.head(), head, "an older clip must not come back");
    }

    #[test]
    fn a_clip_whose_lifetime_ran_out_in_transit_never_lands() {
        let mut peer = clipboard();
        peer.copy(text("hunter2"), Vec::new(), true, Some(Duration::ZERO))
            .unwrap();

        let mut board = clipboard();
        let effects = board.accept(pulled(&peer)).unwrap();

        assert!(applied(&effects).is_empty());
        assert!(board.current().is_none());
    }

    #[test]
    fn a_restored_clip_is_not_put_back_on_the_clipboard() {
        let mut board = clipboard();
        copy(&mut board, "before the restart");

        let mut restarted = Clipboard::new(board.origin, board.settings.clone(), board.saved());
        assert_eq!(current(&restarted).as_deref(), Some("before the restart"));
        assert!(applied(&restarted.expire(SystemTime::now())).is_empty());
        assert!(applied(&restarted.set_pause(None, None)).is_empty());
    }

    #[test]
    fn pausing_capture_stops_sharing_what_is_copied_here() {
        let mut board = clipboard();
        board.set_pause(Some(Switch::Paused { until: None }), None);

        assert!(
            board
                .captured(captured("private", false), Vec::new())
                .is_empty()
        );
        assert!(board.current().is_none());
    }

    #[test]
    fn a_paused_apply_holds_back_until_resumed_or_lapsed() {
        let mut peer = clipboard();
        copy(&mut peer, "first");

        let mut board = clipboard();
        board.set_pause(None, Some(Switch::Paused { until: None }));
        assert!(applied(&board.accept(pulled(&peer)).unwrap()).is_empty());
        assert_eq!(applied(&board.set_pause(None, Some(Switch::On))), ["first"]);

        copy(&mut peer, "second");
        let until = SystemTime::now() + Duration::from_hours(1);
        board.set_pause(None, Some(Switch::Paused { until: Some(until) }));
        assert!(applied(&board.accept(pulled(&peer)).unwrap()).is_empty());
        assert_eq!(applied(&board.expire(until)), ["second"]);
    }

    /// What a browser copy is: the same selection twice over. Both reach
    /// the compositor, and the plain one under every name for text.
    #[test]
    fn every_representation_reaches_the_clipboard() {
        let mut board = clipboard();
        let reps = selection(vec![
            Rep::new("text/plain;charset=utf-8", b"hello".to_vec()),
            Rep::new("text/html", b"<b>hello</b>".to_vec()),
        ]);
        let effects = board.copy(reps, Vec::new(), false, None).unwrap();

        let served = served(&effects);
        assert_eq!(served.len(), 2);
        assert!(served[0].0.contains(&"UTF8_STRING".to_owned()));
        assert_eq!(served[0].1, "hello");
        assert_eq!(served[1].0, vec!["text/html".to_owned()]);
    }

    /// Copying a file copies its path, and a path belongs to the machine
    /// it is on. Handing it back as a file reference anywhere else would
    /// point a file manager at nothing, or at a different file.
    #[test]
    fn a_file_reference_is_only_offered_where_it_came_from() {
        let mut peer = clipboard();
        let reps = selection(vec![
            Rep::new("text/uri-list", b"file:///home/x/y.iso\r\n".to_vec()),
            Rep::new(
                "x-special/gnome-copied-files",
                b"cut\nfile:///home/x/y.iso".to_vec(),
            ),
        ]);
        let effects = peer.copy(reps, Vec::new(), false, None).unwrap();
        assert_eq!(served(&effects)[0].0, vec!["text/uri-list".to_owned()]);

        let mut board = clipboard();
        let served = served(&board.accept(pulled(&peer)).unwrap());
        assert_eq!(served.len(), 1, "neither type survives the trip");
        assert!(served[0].0.contains(&"text/plain".to_owned()));
        assert_eq!(served[0].1, "file:///home/x/y.iso\r\n");
    }

    /// A file copy reaching the clipboard is asked for, once, and holds
    /// the paths as text until it arrives: the clipboard is never held up
    /// for a transfer. Once the files land it is a file again, pointing at
    /// the copies here rather than at a path on the machine that copied it.
    #[test]
    fn a_file_copy_waits_as_text_until_its_files_land() {
        let mut peer = clipboard();
        let selection = Selection::of_files(&[PathBuf::from("/home/x/y.iso")]);
        let files = vec![FileRef {
            path: "y.iso".to_owned(),
            size: 4,
            hash: Hash::of(b"iso!"),
        }];
        peer.copy(selection, files, false, None).unwrap();

        let mut board = clipboard();
        let effects = board.accept(pulled(&peer)).unwrap();
        let fetches = effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Fetch(_)))
            .count();
        assert_eq!(fetches, 1);
        let waiting = served(&effects);
        assert_eq!(waiting.len(), 1, "no file type is offered yet");
        assert!(waiting[0].0.contains(&"text/plain".to_owned()));

        let stamp = board.head().unwrap();
        let effects = board.materialized(stamp, PathBuf::from("/spool/trees/abc"));
        let served = served(&effects);
        let types: Vec<&str> = served.iter().map(|(mimes, _)| mimes[0].as_str()).collect();
        assert_eq!(
            types,
            vec![mime::PLAIN_TEXT, mime::URI_LIST, mime::GNOME_COPIED_FILES],
        );
        assert_eq!(served[0].1, "/spool/trees/abc/y.iso\n");
        assert_eq!(served[1].1, "file:///spool/trees/abc/y.iso\r\n");
        assert_eq!(
            served[2].1, "copy\nfile:///spool/trees/abc/y.iso",
            "a cut pasted here would move the only copy of the content",
        );
    }

    /// A selection whose alternates do not fit is still worth sharing.
    /// What the cap takes is the alternates, from the back.
    #[test]
    fn what_does_not_fit_costs_the_alternates_not_the_copy() {
        let mut board = with_settings(Settings {
            max_size: bytesize::ByteSize::b(16),
            ..Settings::default()
        });
        let reps = selection(vec![
            Rep::new("text/plain", b"hello".to_vec()),
            Rep::new("text/html", vec![b'x'; 32]),
        ]);
        board.captured(
            Captured {
                selection: reps,
                secret: false,
            },
            Vec::new(),
        );

        assert_eq!(board.current().unwrap().1.selection.size(), 5);
        assert!(
            board
                .captured(captured(&"x".repeat(17), false), Vec::new())
                .is_empty()
        );
        assert!(
            board
                .copy(text(&"x".repeat(17)), Vec::new(), false, None)
                .is_err()
        );
    }
}
