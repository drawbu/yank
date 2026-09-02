//! Two machines, end to end.
//!
//! Every test here runs two whole daemons in one process, talking over
//! hermetic iroh endpoints (no relays, no DNS, nothing outside the
//! machine) and driven through their control sockets exactly as the CLI
//! drives them. What is being checked is the behaviour the design is for:
//! that a machine which was away comes back to the right history, and that
//! catching up sets the clipboard once rather than replaying it.
//!
//! The compositor is turned off in every daemon's `config.toml`; what the
//! clipboard state machine does with a compositor is covered by the unit
//! tests in `clip::state`.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use color_eyre::eyre::{Result, bail};
use tempfile::TempDir;
use yank::{
    config::Dirs,
    daemon::{
        Daemon,
        control::{CLIENT_TIMEOUT, Client, FileInfo, HistoryEntry, Request, Response},
    },
    net::EndpointOptions,
};

/// How long a test waits for something to reach the other machine before
/// calling it a failure.
const SETTLE: Duration = Duration::from_secs(20);

/// One machine of the test mesh.
struct Machine {
    dirs: Dirs,
    daemon: Option<Daemon>,
    /// Kept so the directory outlives the machine, restarts included.
    _home: TempDir,
    options: EndpointOptions,
}

impl Machine {
    /// Starts a machine on a fresh directory, reachable by the others
    /// sharing `lookup`.
    async fn start(lookup: &iroh::address_lookup::MemoryLookup) -> Result<Self> {
        let home = tempfile::tempdir()?;
        let dirs = Dirs::new(Some(home.path().to_owned()))?;
        write_settings(home.path())?;

        let options = EndpointOptions::LocalTest {
            lookup: lookup.clone(),
        };
        let daemon = Daemon::start(&dirs, &options).await?;

        Ok(Machine {
            dirs,
            daemon: Some(daemon),
            _home: home,
            options,
        })
    }

    /// Stops the daemon, leaving everything it wrote on disk.
    async fn stop(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown().await;
        }
    }

    /// Starts the daemon again on the same directory.
    async fn restart(&mut self) -> Result<()> {
        self.stop().await;
        self.daemon = Some(Daemon::start(&self.dirs, &self.options).await?);

        Ok(())
    }

    /// Sends one request, the way the CLI does.
    async fn ask(&self, request: &Request) -> Result<Response> {
        self.ask_within(request, CLIENT_TIMEOUT).await
    }

    /// The same, for a request the daemon answers only once something has
    /// happened.
    async fn ask_within(&self, request: &Request, within: Duration) -> Result<Response> {
        Client::connect_required(&self.dirs)
            .await?
            .request(request, within)
            .await
    }

    /// Copies something on this machine.
    async fn copy(&self, text: &str) -> Result<()> {
        self.ask(&Request::Copy {
            mime: "text/plain".to_owned(),
            bytes: text.as_bytes().to_vec(),
            secret: false,
            ttl_secs: None,
        })
        .await?;

        Ok(())
    }

    /// The history, newest first.
    async fn history(&self) -> Result<Vec<HistoryEntry>> {
        match self.ask(&Request::History { limit: None }).await? {
            Response::History(entries) => Ok(entries),
            other => bail!("unexpected answer: {other:?}"),
        }
    }

    /// Copies files on this machine, contents and all.
    async fn copy_files(&self, paths: &[PathBuf]) -> Result<String> {
        let paths = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();

        match self
            .ask(&Request::CopyFiles {
                paths,
                ttl_secs: None,
            })
            .await?
        {
            Response::Wrote { label } => Ok(label),
            other => bail!("unexpected answer: {other:?}"),
        }
    }

    /// Where the files of an entry are on this machine. Asking is what
    /// makes the daemon go and get them, and the answer is what waits.
    async fn files(&self, entry: Option<&str>) -> Result<(Option<PathBuf>, Vec<FileInfo>)> {
        match self
            .ask_within(
                &Request::Files {
                    entry: entry.map(str::to_owned),
                },
                SETTLE,
            )
            .await?
        {
            Response::Files { tree, files, .. } => Ok((tree.map(PathBuf::from), files)),
            other => bail!("unexpected answer: {other:?}"),
        }
    }

    /// What this machine would paste, or `None` when nothing is selected.
    async fn selection(&self) -> Result<Option<String>> {
        Ok(self
            .history()
            .await?
            .into_iter()
            .find(|entry| entry.selected)
            .map(|entry| entry.preview))
    }
}

/// Writes a `config.toml` that keeps the tests off the machine running
/// them: no compositor, and a small history so the caps are exercised.
fn write_settings(home: &Path) -> Result<()> {
    std::fs::write(
        home.join("config.toml"),
        "clipboard = false\nhistory-limit = 10\n",
    )?;

    Ok(())
}

/// Pairs two machines through their control sockets, ticket and all.
async fn pair(host: &Machine, joiner: &Machine) -> Result<()> {
    let ticket = match host
        .ask(&Request::PairHost {
            name: "host".to_owned(),
        })
        .await?
    {
        Response::PairTicket(ticket) => ticket,
        other => bail!("unexpected answer: {other:?}"),
    };

    match joiner
        .ask(&Request::PairJoin {
            ticket,
            name: "joiner".to_owned(),
        })
        .await?
    {
        Response::Paired { .. } => Ok(()),
        other => bail!("unexpected answer: {other:?}"),
    }
}

/// Polls until `check` holds, which is how a test waits for something to
/// cross the mesh without pinning down how long that takes.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let deadline = std::time::Instant::now() + SETTLE;
    loop {
        if check().await.unwrap_or(false) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Two paired machines, ready to use.
async fn mesh() -> Result<(Machine, Machine)> {
    let lookup = iroh::address_lookup::MemoryLookup::default();
    let first = Machine::start(&lookup).await?;
    let second = Machine::start(&lookup).await?;
    pair(&first, &second).await?;

    Ok((first, second))
}

#[tokio::test(flavor = "multi_thread")]
async fn what_one_machine_copies_the_other_can_paste() -> Result<()> {
    let (first, second) = mesh().await?;

    first.copy("from the first").await?;
    eventually("the entry to arrive", || async {
        Ok(second.selection().await? == Some("from the first".to_owned()))
    })
    .await;

    // And back the other way, on the same connection.
    second.copy("from the second").await?;
    eventually("the answer to arrive", || async {
        Ok(first.selection().await? == Some("from the second".to_owned()))
    })
    .await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_machine_that_was_away_catches_up_in_order() -> Result<()> {
    let (first, mut second) = mesh().await?;
    second.stop().await;

    for text in ["one", "two", "three"] {
        first.copy(text).await?;
    }
    second.restart().await?;

    eventually("the backlog to arrive", || async {
        Ok(second.history().await?.len() == 3)
    })
    .await;

    let previews: Vec<String> = second
        .history()
        .await?
        .into_iter()
        .map(|entry| entry.preview)
        .collect();
    assert_eq!(previews, ["three", "two", "one"]);
    // Only the newest is the selection: catching up must not walk the
    // clipboard through everything that was missed.
    assert_eq!(second.selection().await?, Some("three".to_owned()));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_copy_survives_catching_up() -> Result<()> {
    let (first, mut second) = mesh().await?;
    second.stop().await;

    first.copy("copied elsewhere while away").await?;
    // Far enough apart that the clock cannot call them simultaneous.
    tokio::time::sleep(Duration::from_millis(5)).await;

    second.restart().await?;
    second.copy("copied here just now").await?;

    eventually("the backlog to arrive", || async {
        Ok(second.history().await?.len() == 2)
    })
    .await;
    // The older entry joins the history without taking the clipboard.
    assert_eq!(
        second.selection().await?,
        Some("copied here just now".to_owned()),
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_an_entry_removes_it_on_both() -> Result<()> {
    let (first, second) = mesh().await?;

    first.copy("a mistake").await?;
    eventually("the entry to arrive", || async {
        Ok(second.history().await?.len() == 1)
    })
    .await;

    let label = second.history().await?[0].label.clone();
    second.ask(&Request::Forget { entry: label }).await?;

    eventually("the removal to arrive", || async {
        Ok(first.history().await?.is_empty())
    })
    .await;
    assert!(second.history().await?.is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn clearing_reaches_a_machine_that_was_away() -> Result<()> {
    let (first, mut second) = mesh().await?;

    first.copy("something").await?;
    eventually("the entry to arrive", || async {
        Ok(second.selection().await?.is_some())
    })
    .await;

    second.stop().await;
    first.ask(&Request::Clear { history: false }).await?;
    second.restart().await?;

    eventually("the clear to arrive", || async {
        Ok(second.selection().await?.is_none())
    })
    .await;
    // The clipboard is empty, the history is not.
    assert_eq!(second.history().await?.len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_reaches_the_other_machine_but_not_its_disk() -> Result<()> {
    let (first, second) = mesh().await?;

    first
        .ask(&Request::Copy {
            mime: "text/plain".to_owned(),
            bytes: b"hunter2".to_vec(),
            secret: true,
            ttl_secs: Some(600),
        })
        .await?;

    eventually("the secret to arrive", || async {
        Ok(second.history().await?.len() == 1)
    })
    .await;

    let entry = second.history().await?.remove(0);
    assert!(entry.secret);
    assert_eq!(entry.preview, "<secret>");
    assert!(entry.expires_in_secs.is_some());

    // It is pastable on the other machine, which is the point of sharing
    // it, and nowhere in its history directory, which is the point of
    // marking it.
    let Response::Contents { bytes, .. } = second
        .ask(&Request::Paste {
            entry: None,
            mime: None,
        })
        .await?
    else {
        bail!("expected the contents");
    };
    assert_eq!(bytes, b"hunter2");

    let history = std::fs::read_dir(second.dirs.history_dir())?;
    for file in history {
        let bytes = std::fs::read(file?.path())?;
        assert!(
            !bytes.windows(7).any(|window| window == b"hunter2"),
            "a secret was written to disk",
        );
    }

    Ok(())
}

/// A machine that restarts has to tell its peers what it holds without
/// waiting for someone to copy something. Nothing else would make a peer
/// ask for a history written before either of them was last started.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_machine_announces_what_it_already_had() -> Result<()> {
    let (mut first, mut second) = mesh().await?;
    second.stop().await;

    first.copy("written before either restarted").await?;
    first.restart().await?;
    second.restart().await?;

    eventually("the history to arrive", || async {
        Ok(second.history().await?.len() == 1)
    })
    .await;
    assert_eq!(
        second.selection().await?,
        Some("written before either restarted".to_owned()),
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_machine_keeps_its_history() -> Result<()> {
    let lookup = iroh::address_lookup::MemoryLookup::default();
    let mut machine = Machine::start(&lookup).await?;

    machine.copy("written before the restart").await?;
    machine.restart().await?;

    let history = machine.history().await?;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].preview, "written before the restart");

    Ok(())
}

/// The whole point of carrying files rather than paths: what the second
/// machine gets is the bytes, under the names they had, and not a path
/// into a filesystem it does not have.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_copied_on_one_machine_is_a_file_on_the_other() -> Result<()> {
    let (first, second) = mesh().await?;

    let source = tempfile::tempdir()?;
    std::fs::create_dir_all(source.path().join("folder/deep"))?;
    std::fs::write(source.path().join("folder/one.txt"), b"the first file")?;
    std::fs::write(
        source.path().join("folder/deep/two.bin"),
        vec![7u8; 300_000],
    )?;
    let loose = source.path().join("loose.txt");
    std::fs::write(&loose, b"on its own")?;

    let label = first
        .copy_files(&[source.path().join("folder"), loose])
        .await?;

    eventually("the entry to arrive", || async {
        Ok(second.history().await?.iter().any(|entry| entry.files == 3))
    })
    .await;

    // Asking is what sets the fetch going, and the answer is what waits
    // for it: the daemon holds the request until the files are laid out.
    let (tree, files) = second.files(Some(&label)).await?;
    let tree = tree.expect("the answer waits for them");

    let names: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(
        names,
        vec!["folder/deep/two.bin", "folder/one.txt", "loose.txt"],
    );
    assert_eq!(
        std::fs::read(tree.join("folder/one.txt"))?,
        b"the first file",
    );
    assert_eq!(
        std::fs::read(tree.join("folder/deep/two.bin"))?,
        vec![7u8; 300_000],
    );
    assert_eq!(std::fs::read(tree.join("loose.txt"))?, b"on its own");

    Ok(())
}

/// The source is not read again at paste time, so what arrives is what was
/// copied, whatever happened to the original in between.
#[tokio::test(flavor = "multi_thread")]
async fn what_arrives_is_what_was_copied_and_not_what_the_file_became() -> Result<()> {
    let (first, second) = mesh().await?;

    let source = tempfile::tempdir()?;
    let path = source.path().join("notes.txt");
    std::fs::write(&path, b"as it was")?;

    let label = first.copy_files(std::slice::from_ref(&path)).await?;
    std::fs::write(&path, b"edited afterwards")?;

    eventually("the entry to arrive", || async {
        Ok(second.history().await?.iter().any(|it| it.label == label))
    })
    .await;

    let (tree, _) = second.files(Some(&label)).await?;
    assert_eq!(
        std::fs::read(tree.unwrap().join("notes.txt"))?,
        b"as it was",
    );

    Ok(())
}

/// An entry that leaves the history takes its files with it: they are
/// clipboard contents, and they sit on the disk in the clear.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_an_entry_drops_its_files() -> Result<()> {
    let (first, second) = mesh().await?;

    let source = tempfile::tempdir()?;
    let path = source.path().join("secret.txt");
    std::fs::write(&path, b"nothing to see")?;
    let label = first.copy_files(std::slice::from_ref(&path)).await?;

    eventually("the entry to arrive", || async {
        Ok(second.history().await?.iter().any(|it| it.label == label))
    })
    .await;
    let (tree, _) = second.files(Some(&label)).await?;
    let tree = tree.expect("the answer waits for them");

    first.ask(&Request::Forget { entry: label }).await?;
    eventually("the files to go", || async { Ok(!tree.exists()) }).await;

    let spool = second.dirs.files_dir().join("blobs");
    let left = std::fs::read_dir(spool)?.count();
    assert_eq!(left, 0, "the content of a forgotten entry stays on disk");

    Ok(())
}
