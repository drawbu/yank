//! Two machines, end to end.
//!
//! Every test here runs whole daemons in one process, talking over
//! hermetic iroh endpoints (no relays, no DNS, nothing outside the
//! machine) and driven through their control sockets exactly as the CLI
//! drives them. What is being checked is the behaviour the design is for:
//! whichever copy is the latest ends up on every machine, including one
//! that was away when it was made.
//!
//! The compositor is turned off in every daemon's `config.toml`; what the
//! clipboard state machine does with a compositor is covered by the unit
//! tests in `clip::state`.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use iroh::address_lookup::MemoryLookup;
use tempfile::TempDir;
use yank::{
    cli,
    config::Dirs,
    daemon::{
        Daemon,
        control::{ConnectionStatus, Request, Response},
    },
    files::FileRef,
};

const SETTLE: Duration = Duration::from_secs(20);

struct Machine {
    dirs: Dirs,
    daemon: Option<Daemon>,
    /// Kept so the directory outlives the machine, restarts included.
    home: TempDir,
    lookup: MemoryLookup,
}

impl Machine {
    async fn start(lookup: &MemoryLookup) -> eyre::Result<Self> {
        let home = tempfile::tempdir()?;
        let dirs = Dirs::new(Some(home.path().to_owned()))?;
        // Keeps the tests off the compositor of the machine running them.
        std::fs::write(home.path().join("config.toml"), "clipboard = false\n")?;

        let daemon = Daemon::start(&dirs, Some(lookup)).await?;

        Ok(Machine {
            dirs,
            daemon: Some(daemon),
            home,
            lookup: lookup.clone(),
        })
    }

    async fn stop(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown().await;
        }
    }

    async fn restart(&mut self) -> eyre::Result<()> {
        self.stop().await;
        self.daemon = Some(Daemon::start(&self.dirs, Some(&self.lookup)).await?);

        Ok(())
    }

    async fn ask(&self, request: &Request) -> eyre::Result<Response> {
        cli::ask(&self.dirs, request).await
    }

    async fn copy(&self, text: &str) -> eyre::Result<()> {
        self.ask(&Request::Copy {
            mime: "text/plain".to_owned(),
            bytes: text.as_bytes().to_vec(),
            secret: false,
            ttl: None,
        })
        .await?;

        Ok(())
    }

    async fn copy_files(&self, paths: &[PathBuf]) -> eyre::Result<()> {
        let paths = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        self.ask(&Request::CopyFiles { paths, ttl: None }).await?;

        Ok(())
    }

    /// Where the clipboard's files are on this machine, once a clip naming
    /// some arrived. Asking is what makes the daemon go and get them, and
    /// the answer is what waits.
    async fn files(&self) -> eyre::Result<(PathBuf, Vec<FileRef>)> {
        eventually("a clip naming files to arrive", || async {
            match self.ask(&Request::Status).await? {
                Response::Status(status) => {
                    Ok(status.clipboard.current.is_some_and(|clip| clip.files > 0))
                }
                _ => Ok(false),
            }
        })
        .await;

        match self.ask(&Request::Files).await? {
            Response::Files {
                tree: Some(tree),
                files,
            } => Ok((PathBuf::from(tree), files)),
            other => eyre::bail!("unexpected answer: {other:?}"),
        }
    }

    /// The preview of what the clipboard holds, as `yank status` shows it.
    async fn current(&self) -> eyre::Result<Option<String>> {
        match self.ask(&Request::Status).await? {
            Response::Status(status) => Ok(status.clipboard.current.map(|clip| clip.preview)),
            other => eyre::bail!("unexpected answer: {other:?}"),
        }
    }

    async fn paste(&self) -> eyre::Result<Vec<u8>> {
        match self.ask(&Request::Paste { mime: None }).await? {
            Response::Contents { bytes, .. } => Ok(bytes.to_vec()),
            other => eyre::bail!("unexpected answer: {other:?}"),
        }
    }

    /// Issues a pairing ticket, naming this machine `name`.
    async fn ticket(&self, name: &str) -> eyre::Result<String> {
        let name = name.to_owned();
        match self.ask(&Request::PairHost { name }).await? {
            Response::PairTicket(ticket) => Ok(ticket),
            other => eyre::bail!("unexpected answer: {other:?}"),
        }
    }

    /// Redeems a ticket, naming this machine `name`.
    async fn join(&self, ticket: String, name: &str) -> eyre::Result<()> {
        let name = name.to_owned();
        match self.ask(&Request::PairJoin { ticket, name }).await? {
            Response::Paired(_) => Ok(()),
            other => eyre::bail!("unexpected answer: {other:?}"),
        }
    }

    /// The machines this one is paired with, and whether each is connected.
    async fn peers(&self) -> eyre::Result<Vec<(String, bool)>> {
        match self.ask(&Request::Status).await? {
            Response::Status(status) => Ok(status
                .peers
                .into_iter()
                .map(|peer| {
                    let connected = matches!(peer.connection, ConnectionStatus::Connected { .. });
                    (peer.name, connected)
                })
                .collect()),
            other => eyre::bail!("unexpected answer: {other:?}"),
        }
    }

    /// Waits until the clipboard previews as `expected`.
    async fn holds(&self, expected: Option<&str>) {
        eventually(&format!("the clipboard to hold {expected:?}"), || async {
            Ok(self.current().await?.as_deref() == expected)
        })
        .await;
    }
}

/// Pairs two machines through their control sockets, ticket and all.
async fn pair(host: &Machine, joiner: &Machine) -> eyre::Result<()> {
    joiner.join(host.ticket("host").await?, "joiner").await
}

/// Polls until `check` holds, which is how a test waits for something to
/// cross the mesh without pinning down how long that takes.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = eyre::Result<bool>>,
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

async fn mesh() -> eyre::Result<(Machine, Machine)> {
    let lookup = MemoryLookup::default();
    let first = Machine::start(&lookup).await?;
    let second = Machine::start(&lookup).await?;
    pair(&first, &second).await?;

    Ok((first, second))
}

/// Every file under `dir`, read whole.
fn every_file(dir: &Path) -> Vec<Vec<u8>> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn what_one_machine_copies_the_other_can_paste() -> eyre::Result<()> {
    let (first, second) = mesh().await?;

    first.copy("from the first").await?;
    second.holds(Some("from the first")).await;
    assert_eq!(second.paste().await?, b"from the first");

    // And back the other way, on the same connection.
    second.copy("from the second").await?;
    first.holds(Some("from the second")).await;

    Ok(())
}

/// A ticket pairs one machine, once: one revoked by a newer ticket, or
/// already redeemed, pairs nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_pairs_once_and_only_while_outstanding() -> eyre::Result<()> {
    let lookup = MemoryLookup::default();
    let host = Machine::start(&lookup).await?;
    let joiner = Machine::start(&lookup).await?;
    let stranger = Machine::start(&lookup).await?;

    let revoked = host.ticket("host").await?;
    let outstanding = host.ticket("host").await?;
    assert!(joiner.join(revoked, "joiner").await.is_err());
    assert!(host.peers().await?.is_empty());
    assert!(joiner.peers().await?.is_empty());

    joiner.join(outstanding.clone(), "joiner").await?;
    assert!(stranger.join(outstanding, "stranger").await.is_err());
    assert!(stranger.peers().await?.is_empty());
    eventually("the host to list the joiner alone", || async {
        let peers = host.peers().await?;
        Ok(peers.len() == 1 && peers[0].0 == "joiner")
    })
    .await;

    Ok(())
}

/// Machines that only paired with a third learn of each other through
/// its gossip, connect, and share without it.
#[tokio::test(flavor = "multi_thread")]
async fn machines_paired_through_a_third_find_each_other() -> eyre::Result<()> {
    let lookup = MemoryLookup::default();
    let mut middle = Machine::start(&lookup).await?;
    let first = Machine::start(&lookup).await?;
    let second = Machine::start(&lookup).await?;
    first.join(middle.ticket("middle").await?, "first").await?;
    second
        .join(middle.ticket("middle").await?, "second")
        .await?;

    let connected = |peers: Vec<(String, bool)>, other: &str| {
        peers.iter().any(|(name, up)| name == other && *up)
    };
    eventually("the two to connect to each other", || async {
        Ok(connected(first.peers().await?, "second") && connected(second.peers().await?, "first"))
    })
    .await;

    middle.stop().await;
    first.copy("past the third").await?;
    second.holds(Some("past the third")).await;

    Ok(())
}

/// A removed machine is cut off: the remover forgets it, the connection
/// goes, and what is copied afterwards no longer reaches it.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_machine_no_longer_shares() -> eyre::Result<()> {
    let (first, second) = mesh().await?;
    first.copy("before").await?;
    second.holds(Some("before")).await;

    first
        .ask(&Request::RemovePeer {
            peer: "joiner".to_owned(),
        })
        .await?;
    eventually("the remover to forget the machine", || async {
        Ok(first.peers().await?.is_empty())
    })
    .await;
    eventually("the removed machine to lose its connection", || async {
        Ok(second
            .peers()
            .await?
            .iter()
            .all(|(_, connected)| !connected))
    })
    .await;

    // Each copy is the latest on its own side, so either one crossing
    // would show.
    second.copy("from the removed machine").await?;
    first.copy("after").await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        second.current().await?.as_deref(),
        Some("from the removed machine")
    );
    assert_eq!(first.current().await?.as_deref(), Some("after"));

    Ok(())
}

/// Two machines that copied while apart settle on the later copy, the one
/// holding the older reconnecting last.
#[tokio::test(flavor = "multi_thread")]
async fn the_later_copy_wins_after_a_split() -> eyre::Result<()> {
    let (mut first, mut second) = mesh().await?;

    second.stop().await;
    first.copy("older, made while apart").await?;
    first.stop().await;

    second.restart().await?;
    second.copy("newer, made while apart").await?;
    first.restart().await?;

    first.holds(Some("newer, made while apart")).await;
    assert_eq!(
        second.current().await?.as_deref(),
        Some("newer, made while apart"),
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_reaches_the_other_machine_but_not_its_disk() -> eyre::Result<()> {
    let (first, second) = mesh().await?;

    first
        .ask(&Request::Copy {
            mime: "text/plain".to_owned(),
            bytes: b"hunter2".to_vec(),
            secret: true,
            ttl: Some(Duration::from_secs(3)),
        })
        .await?;

    second.holds(Some("<secret>")).await;
    assert_eq!(second.paste().await?, b"hunter2");

    for machine in [&first, &second] {
        for bytes in every_file(machine.home.path()) {
            assert!(
                !bytes.windows(7).any(|window| window == b"hunter2"),
                "a secret was written to disk",
            );
        }
    }

    // Gone everywhere once its lifetime is up, network or not.
    first.holds(None).await;
    second.holds(None).await;

    Ok(())
}

/// A machine alone keeps its clipboard across a restart: nothing but its
/// own disk can give it back.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_keeps_the_clipboard() -> eyre::Result<()> {
    let mut machine = Machine::start(&MemoryLookup::default()).await?;

    machine.copy("before the restart").await?;
    machine.restart().await?;

    assert_eq!(
        machine.current().await?.as_deref(),
        Some("before the restart")
    );

    Ok(())
}

/// A machine that was away gets the latest clip from one that restarted
/// since, which has to announce what it holds without waiting for a new
/// copy, and must not bring an older one back.
#[tokio::test(flavor = "multi_thread")]
async fn a_clear_reaches_a_machine_that_was_away_across_restarts() -> eyre::Result<()> {
    let (mut first, mut second) = mesh().await?;

    first.copy("something").await?;
    second.holds(Some("something")).await;

    second.stop().await;
    first.ask(&Request::Clear).await?;
    first.restart().await?;
    second.restart().await?;

    second.holds(None).await;
    assert_eq!(first.current().await?, None);

    Ok(())
}

/// The whole point of carrying files rather than paths: what the second
/// machine gets is the bytes, as they were copied, under the names they
/// had, and not a path into a filesystem it does not have.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_copied_on_one_machine_is_a_file_on_the_other() -> eyre::Result<()> {
    let (first, second) = mesh().await?;

    let source = tempfile::tempdir()?;
    std::fs::create_dir_all(source.path().join("folder/deep"))?;
    std::fs::write(source.path().join("folder/one.txt"), b"as it was")?;
    std::fs::write(
        source.path().join("folder/deep/two.bin"),
        vec![7u8; 300_000],
    )?;
    let loose = source.path().join("loose.txt");
    std::fs::write(&loose, b"on its own")?;

    first
        .copy_files(&[source.path().join("folder"), loose])
        .await?;
    // The source is not read again at paste time.
    std::fs::write(source.path().join("folder/one.txt"), b"edited afterwards")?;

    let (tree, files) = second.files().await?;

    let names: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(
        names,
        vec!["folder/deep/two.bin", "folder/one.txt", "loose.txt"],
    );
    assert_eq!(std::fs::read(tree.join("folder/one.txt"))?, b"as it was");
    assert_eq!(
        std::fs::read(tree.join("folder/deep/two.bin"))?,
        vec![7u8; 300_000],
    );
    assert_eq!(std::fs::read(tree.join("loose.txt"))?, b"on its own");

    Ok(())
}

/// A folder of many small files, some sharing their contents, arrives
/// whole, its contents spooled once each.
#[tokio::test(flavor = "multi_thread")]
async fn a_folder_of_many_files_arrives_whole() -> eyre::Result<()> {
    let (first, second) = mesh().await?;

    let source = tempfile::tempdir()?;
    let folder = source.path().join("many");
    std::fs::create_dir(&folder)?;
    for i in 0..1000 {
        std::fs::write(
            folder.join(format!("{i:04}.txt")),
            format!("file {}", i % 10),
        )?;
    }
    first.copy_files(&[folder]).await?;

    let (tree, files) = second.files().await?;
    assert_eq!(files.len(), 1000);
    assert_eq!(std::fs::read(tree.join("many/0123.txt"))?, b"file 3");
    assert_eq!(
        std::fs::read_dir(second.dirs.files_dir().join("blobs"))?.count(),
        10
    );

    Ok(())
}

/// A machine missing one of the files still sends the others after it, a
/// transfer cut short carries on where it stopped, and the missing file
/// arrives once a machine has it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_resumes_and_skips_what_a_peer_lacks() -> eyre::Result<()> {
    let (first, mut second) = mesh().await?;
    second.stop().await;

    let source = tempfile::tempdir()?;
    let contents: [&[u8]; 3] = [b"first file", b"second file", &[9u8; 200_000]];
    let paths: Vec<PathBuf> = contents
        .iter()
        .enumerate()
        .map(|(i, bytes)| {
            let path = source.path().join(format!("{i}.bin"));
            std::fs::write(&path, bytes)?;
            Ok(path)
        })
        .collect::<std::io::Result<_>>()?;
    first.copy_files(&paths).await?;

    let blob = |machine: &Machine, bytes: &[u8]| {
        let name = yank::files::Hash::of(bytes).file_name();
        machine.dirs.files_dir().join("blobs").join(name)
    };
    // The first machine lost the middle one, and the second already holds
    // the start of the last one.
    let held = blob(&first, contents[1]);
    let kept = std::fs::read(&held)?;
    std::fs::remove_file(&held)?;
    std::fs::create_dir_all(second.dirs.files_dir().join("blobs"))?;
    let partial = blob(&second, contents[2]).with_extension("part");
    std::fs::write(&partial, &contents[2][..50_000])?;

    second.restart().await?;
    eventually("the files the peer has to arrive", || async {
        Ok(blob(&second, contents[0]).exists() && blob(&second, contents[2]).exists())
    })
    .await;

    std::fs::write(&held, kept)?;
    let (tree, _) = second.files().await?;
    for (i, bytes) in contents.iter().enumerate() {
        assert_eq!(std::fs::read(tree.join(format!("{i}.bin")))?, *bytes);
    }

    Ok(())
}

/// A copy that is replaced takes its files with it: they are clipboard
/// contents, and they sit on the disk in the clear.
#[tokio::test(flavor = "multi_thread")]
async fn a_replaced_file_copy_drops_its_files() -> eyre::Result<()> {
    let (first, second) = mesh().await?;

    let source = tempfile::tempdir()?;
    let path = source.path().join("private.txt");
    std::fs::write(&path, b"nothing to see")?;
    first.copy_files(std::slice::from_ref(&path)).await?;
    let (tree, _) = second.files().await?;

    first.copy("something else").await?;
    eventually("the files to go", || async { Ok(!tree.exists()) }).await;

    for machine in [&first, &second] {
        let spool = machine.dirs.files_dir().join("blobs");
        eventually("the spool to empty", || async {
            Ok(std::fs::read_dir(&spool)?.count() == 0)
        })
        .await;
    }

    Ok(())
}
