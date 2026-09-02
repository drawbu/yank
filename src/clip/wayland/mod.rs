//! The Wayland side of the clipboard.
//!
//! Reading and writing the selection without a window means speaking
//! `ext-data-control-v1`, or `wlr-data-control-unstable-v1` on compositors
//! that only have the older one. Both are bound through the `protocol` module.
//!
//! Wayland is a synchronous, poll-driven protocol and yank is otherwise
//! asynchronous, so the connection lives on its own thread and the two
//! sides talk over channels:
//!
//! ```text
//!  daemon (tokio)                       backend thread
//!     │  Command ──── channel ────────►  poll([wayland, wake])
//!     │           └── wake pipe ──────►     │
//!     │◄───────── Event channel ────────────┘
//! ```
//!
//! Nothing that can block runs on that thread: handing our bytes to a
//! pasting application, and reading somebody else's selection, each get a
//! short-lived thread, so an application that stalls mid-transfer costs a
//! thread rather than the whole clipboard.

pub(crate) mod protocol;

use std::{
    collections::HashMap,
    io::{Read as _, Write as _},
    os::fd::{AsFd as _, OwnedFd},
    sync::mpsc,
    thread,
};

use color_eyre::eyre::{Result, WrapErr as _, eyre};
use rustix::event::{PollFd, PollFlags, poll};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, warn};
use wayland_client::{
    Connection, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry::WlRegistry, wl_seat::WlSeat},
};

use self::protocol::{Device, DeviceEvent, Manager, Offer, Source, SourceEvent};
use super::{
    backend::{self, Captured, Command, Event},
    mime,
};
use crate::log::Payload;

/// The Wayland clipboard, as the daemon holds it. Dropping it stops the
/// thread.
#[derive(Debug)]
pub struct Wayland {
    commands: Option<mpsc::Sender<Command>>,
    /// Writing to this wakes the poll in the backend thread; without it a
    /// command would sit in the channel until Wayland happened to say
    /// something.
    wake: OwnedFd,
    thread: Option<thread::JoinHandle<()>>,
}

impl Wayland {
    /// Connects to the compositor and starts serving. `max_bytes` caps
    /// what a single capture may read, so an application offering a
    /// gigabyte cannot be used to exhaust our memory.
    pub fn connect(events: &tokio_mpsc::UnboundedSender<Event>, max_bytes: usize) -> Result<Self> {
        let (wake_rx, wake) = rustix::pipe::pipe().wrap_err("cannot create the wake pipe")?;
        let (commands, queue) = mpsc::channel();
        let (ready, started) = mpsc::channel();

        let events = events.clone();
        let thread = thread::Builder::new()
            .name("yank-wayland".to_owned())
            .spawn(move || match Session::connect(events.clone(), max_bytes) {
                Ok(mut session) => {
                    let _ = ready.send(Ok(()));
                    let reason = match session.run(&wake_rx, &queue) {
                        Ok(()) => "the clipboard backend stopped".to_owned(),
                        Err(err) => format!("{err:#}"),
                    };
                    let _ = events.send(Event::Lost(reason));
                }
                Err(err) => {
                    let _ = ready.send(Err(err));
                }
            })
            .wrap_err("cannot start the Wayland thread")?;

        match started.recv() {
            Ok(Ok(())) => Ok(Wayland {
                commands: Some(commands),
                wake,
                thread: Some(thread),
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(eyre!("the Wayland thread stopped before it started")),
        }
    }
}

impl backend::Backend for Wayland {
    /// Queues a command, waking the backend thread to run it.
    fn send(&self, command: Command) {
        let Some(commands) = &self.commands else {
            return;
        };
        if commands.send(command).is_err() {
            debug!("the Wayland backend is gone; dropping the command");
            return;
        }

        let _ = rustix::io::write(&self.wake, &[0u8]);
    }
}

impl Drop for Wayland {
    fn drop(&mut self) {
        // Closing the queue is what ends the loop; the wake makes the
        // thread notice now rather than at the next Wayland event.
        self.commands = None;
        let _ = rustix::io::write(&self.wake, &[0u8]);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Everything the backend thread owns.
struct Session {
    connection: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
}

/// The dispatch target: what the protocol handlers act on.
struct State {
    manager: Manager,
    device: Device,
    /// Mime types announced so far, per offer, until the offer becomes the
    /// selection or is superseded.
    offers: HashMap<Offer, Vec<String>>,
    /// The offer the current selection came from, kept alive until the
    /// next selection replaces it.
    current: Option<Offer>,
    /// The selection we own, if any.
    held: Option<Held>,
    events: tokio_mpsc::UnboundedSender<Event>,
    max_bytes: usize,
    /// Cleared when the compositor withdraws the device, which ends the
    /// loop.
    alive: bool,
    /// Set when a selection has to be read, so the read is issued once the
    /// dispatch that decided it has let go of the state.
    pending_read: Option<PendingRead>,
}

/// A selection the handler decided to read.
struct PendingRead {
    offer: Offer,
    mime: String,
    secret: bool,
}

/// The selection this daemon owns.
struct Held {
    source: Source,
    bytes: Payload,
}

protocol::impl_manager_dispatch!(State);
protocol::impl_device_dispatch!(State);
protocol::impl_offer_dispatch!(State);
protocol::impl_source_dispatch!(State);

impl wayland_client::Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Session {
    /// Connects, binds the globals and picks up the selection already on
    /// the clipboard.
    fn connect(events: tokio_mpsc::UnboundedSender<Event>, max_bytes: usize) -> Result<Self> {
        let connection = Connection::connect_to_env()
            .wrap_err("cannot connect to the Wayland compositor (is WAYLAND_DISPLAY set?)")?;
        let (globals, queue) = registry_queue_init::<State>(&connection)
            .wrap_err("cannot read the Wayland globals")?;
        let qh = queue.handle();

        // ext-data-control is the standardized protocol; wlr-data-control
        // is what compositors had before it, and many still only have.
        let manager = globals
            .bind(&qh, 1..=1, ())
            .map(Manager::Ext)
            .or_else(|_| globals.bind(&qh, 1..=1, ()).map(Manager::Wlr))
            .map_err(|_| {
                eyre!(
                    "the compositor supports neither ext-data-control-v1 nor \
                     wlr-data-control-unstable-v1, so yank cannot read the clipboard"
                )
            })?;
        let seat: WlSeat = globals
            .bind(&qh, 1..=9, ())
            .map_err(|err| eyre!("the compositor has no seat: {err}"))?;
        let device = manager.device(&seat, &qh);
        debug!("clipboard backend using {}", manager.protocol());

        let mut session = Session {
            state: State {
                manager,
                device,
                offers: HashMap::new(),
                current: None,
                held: None,
                events,
                max_bytes,
                alive: true,
                pending_read: None,
            },
            connection,
            queue,
        };

        // The compositor announces the current selection as soon as the
        // device exists; this is what picks it up on startup.
        session
            .queue
            .roundtrip(&mut session.state)
            .wrap_err("cannot talk to the Wayland compositor")?;
        session.start_pending_read();

        Ok(session)
    }

    /// Serves the clipboard until the command channel closes or the
    /// compositor withdraws the protocol.
    fn run(&mut self, wake: &OwnedFd, commands: &mpsc::Receiver<Command>) -> Result<()> {
        while self.state.alive {
            self.queue.flush()?;
            self.queue.dispatch_pending(&mut self.state)?;
            self.start_pending_read();

            // The guard is taken *before* polling: an event arriving
            // between the dispatch above and the poll below would
            // otherwise go unnoticed until the next one.
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let mut fds = [
                PollFd::new(&self.connection, PollFlags::IN),
                PollFd::new(wake, PollFlags::IN),
            ];
            poll(&mut fds, None).wrap_err("cannot wait on the Wayland connection")?;

            if fds[0].revents().contains(PollFlags::IN) {
                guard.read().wrap_err("the Wayland connection failed")?;
            } else {
                drop(guard);
            }
            if fds[1].revents().contains(PollFlags::IN) {
                let mut drain = [0u8; 64];
                let _ = rustix::io::read(wake, &mut drain);
            }

            self.queue.dispatch_pending(&mut self.state)?;
            self.start_pending_read();

            // A closed command channel means the daemon dropped us.
            if self.run_commands(commands).is_err() {
                return Ok(());
            }
        }

        Err(eyre!("the compositor withdrew the clipboard protocol"))
    }

    /// Applies whatever the daemon queued. Errors only when the channel is
    /// gone for good.
    fn run_commands(&mut self, commands: &mpsc::Receiver<Command>) -> Result<(), ()> {
        loop {
            match commands.try_recv() {
                Ok(Command::Offer { mimes, bytes }) => {
                    self.state.offer(&self.queue.handle(), &mimes, bytes);
                }
                Ok(Command::Clear) => self.state.clear(),
                Err(mpsc::TryRecvError::Empty) => return Ok(()),
                Err(mpsc::TryRecvError::Disconnected) => return Err(()),
            }
        }
    }

    /// Issues the read a selection event asked for.
    ///
    /// The request has to be flushed before the reading thread sees
    /// anything, and the handler that decided to read cannot flush: the
    /// queue is dispatching into the very state it holds.
    fn start_pending_read(&mut self) {
        let Some(pending) = self.state.pending_read.take() else {
            return;
        };

        let (read, write) = match rustix::pipe::pipe() {
            Ok(pipe) => pipe,
            Err(err) => return warn!("cannot create a clipboard pipe: {err}"),
        };
        pending.offer.receive(pending.mime.clone(), write.as_fd());
        // The compositor hands the write end to the source application;
        // our copy has to go, or the read never reaches an end.
        drop(write);
        if let Err(err) = self.connection.flush() {
            return warn!("cannot ask for the clipboard contents: {err}");
        }

        let events = self.state.events.clone();
        let max_bytes = self.state.max_bytes;
        let spawned = thread::Builder::new()
            .name("yank-clipboard-read".to_owned())
            .spawn(move || match read_all(read, max_bytes) {
                Ok(bytes) if bytes.is_empty() => {}
                Ok(bytes) => {
                    let _ = events.send(Event::Copied(Captured {
                        mime: pending.mime,
                        bytes,
                        secret: pending.secret,
                    }));
                }
                Err(err) => debug!("cannot read the clipboard: {err:#}"),
            });
        if let Err(err) = spawned {
            warn!("cannot read the clipboard: {err}");
        }
    }
}

impl State {
    /// Handles a device event: a new offer, a new selection, or the end.
    fn on_device(&mut self, event: DeviceEvent) {
        match event {
            DeviceEvent::NewOffer(offer) => {
                self.offers.insert(offer, Vec::new());
            }
            DeviceEvent::Selection(Some(offer)) => {
                let mimes = self.offers.remove(&offer).unwrap_or_default();
                self.forget_stale_offers();
                self.current = Some(offer.clone());

                // Our own selection comes back to us as an event like any
                // other; the marker is what tells them apart, and without
                // it every machine would copy every entry back.
                if mimes.iter().any(|mime| mime == mime::MARKER) {
                    return;
                }
                let secret = mimes.iter().any(|mime| mime == mime::SECRET_HINT);
                let Some(mime) = mime::choose(&mimes) else {
                    debug!("ignoring a selection with no usable type: {mimes:?}");
                    return;
                };

                self.pending_read = Some(PendingRead {
                    offer,
                    mime: mime.to_owned(),
                    secret,
                });
            }
            DeviceEvent::Selection(None) => {
                self.forget_stale_offers();
                let _ = self.events.send(Event::Emptied);
            }
            DeviceEvent::Finished => self.alive = false,
        }
    }

    /// Collects a mime type announced by an offer.
    fn on_offer_mime(&mut self, offer: &Offer, mime: String) {
        if let Some(mimes) = self.offers.get_mut(offer) {
            mimes.push(mime);
        }
    }

    /// Handles an event about the selection we own.
    fn on_source(&mut self, source: &Source, event: SourceEvent) {
        let Some(held) = &self.held else { return };
        if !held.source.is(source) {
            return;
        }

        match event {
            SourceEvent::Send { mime, fd } => {
                let bytes = held.bytes.clone();
                let spawned = thread::Builder::new()
                    .name("yank-clipboard-write".to_owned())
                    .spawn(move || {
                        if let Err(err) = write_all(fd, &bytes) {
                            debug!("cannot serve the clipboard as {mime}: {err:#}");
                        }
                    });
                if let Err(err) = spawned {
                    warn!("cannot serve the clipboard: {err}");
                }
            }
            // Another application took the selection. What replaced it
            // arrives separately, as a selection event.
            SourceEvent::Cancelled => {
                held.source.destroy();
                self.held = None;
            }
        }
    }

    /// Takes the selection, serving `bytes` under every type in `mimes`.
    fn offer(&mut self, qh: &QueueHandle<Self>, mimes: &[String], bytes: Payload) {
        let source = self.manager.source(qh);
        for mime in mimes {
            source.offer(mime.clone());
        }
        source.offer(mime::MARKER.to_owned());

        self.device.set_selection(Some(&source));
        if let Some(previous) = self.held.replace(Held { source, bytes }) {
            previous.source.destroy();
        }
    }

    /// Empties the selection, whoever owns it.
    fn clear(&mut self) {
        self.device.set_selection(None);
        if let Some(held) = self.held.take() {
            held.source.destroy();
        }
    }

    /// Releases the offers the compositor superseded: an offer we never
    /// use still holds a server-side object until we say otherwise.
    fn forget_stale_offers(&mut self) {
        for (offer, _) in self.offers.drain() {
            offer.destroy();
        }
        if let Some(previous) = self.current.take() {
            previous.destroy();
        }
    }
}

/// Reads a transfer to its end, giving up past `limit` bytes.
fn read_all(fd: OwnedFd, limit: usize) -> Result<Vec<u8>> {
    let mut file = std::fs::File::from(fd);
    let mut bytes = Vec::new();

    // One byte past the limit is enough to know the transfer is too big.
    let read = (&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .wrap_err("the source stopped sending")?;
    if read > limit {
        return Err(eyre!("the selection is larger than the {limit} byte limit"));
    }

    Ok(bytes)
}

/// Hands our bytes to a pasting application, ignoring the pipe closing
/// early: an application is free to read only what it wants.
fn write_all(fd: OwnedFd, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::File::from(fd);
    match file.write_all(bytes) {
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.wrap_err("cannot write the selection"),
    }
}
