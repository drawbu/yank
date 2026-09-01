# yank

`yank` is a peer-to-peer clipboard daemon. It replicates the clipboard across
your machines over a private mesh (using [iroh](https://www.iroh.computer/)),
with no server in between.

**Every machine holds the same clipboard and the same recent history.**
Machines pair once and connect directly from then on:

- What you copy on one machine is on the others as soon as they are
  reachable, and can be pasted there like anything else.
- The history is shared, so an entry copied an hour ago on another machine
  is still there to pick from.
- A machine that was off, asleep or offline catches up when it comes back.
  Its clipboard is set once, to the most recent entry, rather than walked
  through everything it missed.
- Entries can be given a lifetime. `yank copy --secret` shares a password
  and takes it back everywhere ninety seconds later, without writing it to
  disk on any machine.

```console
$ yank status
This machine 66a7e0c4ab0aa85615819f5257ad995e5b6f1cef3fc6e0bf412e818f7c8ea559
  yank 0.1.0, up for 2h 14m
  clipboard connected
  Sharing in both directions

Clipboard
  ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample user@host — desktop, 41s ago
  62 entries shared

Machines
  desktop  connected direct [2a01:cb19:57:7500:a050:1baa:1f9a:c74c]:57148, 3ms, for 2h 13m
```

`yank` is meant for the machines one person owns. It reads the clipboard
through `ext-data-control-v1`, or `wlr-data-control-unstable-v1` on
compositors that only have the older one, so it works on Sway, niri,
Hyprland, KDE and most others. There is no X11 backend. A machine with no
graphical session can still take part: it replicates, and `yank copy` and
`yank paste` work there.

### GNOME and macOS

Neither is supported yet.

GNOME implements neither data-control protocol, and the [clipboard
portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Clipboard.html)
extends remote-desktop sessions rather than standing on its own, so there
is currently no interface a clipboard manager can use. These protocols let
any client read every clipboard change without asking, which is a fair
thing to weigh against; see [mutter#524](https://gitlab.gnome.org/GNOME/mutter/-/issues/524).
Reaching GNOME would mean a Shell extension rather than another Wayland
backend.

macOS is possible and simply not written, the project is still brand new.
I do not own a macOS machine, but I do not object to write support some day if
there is some demand.

## Installation

`yank` is not distributed by any package manager yet. With `cargo`:

```sh
$ cargo install --git https://github.com/drawbu/yank.git --locked
$ yank service install
```

With [Nix](https://nixos.org/), the flake exposes a Home Manager module
that installs `yank` and runs the daemon as a user service:

```nix
{
  imports = [ inputs.yank.homeModules.default ];

  services.yank = {
    enable = true;
    # Optional, to manage config.toml declaratively:
    settings = { };
  };
}
```

`nixosModules.default` does the same for systems without Home Manager,
leaving `config.toml` to the user. Do not also run `yank service install`
there: it would write a second unit shadowing the one Nix wrote.

## Usage

Pair the machines. Run this on one, and what it prints on the other:

```sh
$ yank peer add
```

A paired machine sees everything you copy and can pair further machines
itself, so pair only machines you own. After that the clipboard is shared
and nothing else is needed.

```sh
$ echo hi | yank copy       # copy from a pipe, like wl-copy
$ yank paste                # print the clipboard, for pipes
$ yank list                 # the shared history, newest first
$ yank pick abc123-4        # put one of those back on the clipboard
$ yank rm abc123-4          # remove one entry from every machine
$ yank clear [--history]    # empty every machine's clipboard
$ yank copy --secret        # gone everywhere in 90 seconds
$ yank pause [--capture|--apply] [--for 30m]
$ yank resume
```

Password managers that mark what they copy with
`x-kde-passwordManagerHint`, 1Password and KeePassXC among them, get the
`--secret` treatment without being asked.

Configuration lives in `~/.config/yank/config.toml`, written with every
option commented out on first start. The daemon reads it once, so run
`yank service restart` after editing it.

## How it works

```text
┌─────┐  control  ┌────────┐    iroh (QUIC)    ┌──────────────┐
│ CLI │──socket──►│ daemon │◄─────────────────►│ peer daemons │
└─────┘           └───┬────┘                   └──────────────┘
                      │ ext/wlr-data-control
                      ▼
                Wayland compositor
```

What machines replicate is an append-only log. Each writes entries under
its own identity, numbered and never renumbered, so receiving one twice is
a no-op and there is nothing to reconcile. Machines announce what they hold
and pull what they lack, which is why catching up on a day of history takes
the same path as syncing live.

The clipboard is *derived* from that log rather than tracked: the history
is every entry not forgotten, the selection is the newest entry written
after the last clear. Two machines that have seen the same entries agree,
whatever order those arrived in.

Entries are opaque below `src/clip`, so the clipboard is the first thing
the log carries rather than the only thing it could. Start reading at
[`src/lib.rs`](src/lib.rs), which maps the modules; each one documents what
it owns and why it is arranged as it is.

## Security and privacy

`yank` uses iroh to connect machines directly, hole-punching where the
network allows it and falling back to iroh's public relays otherwise.
Connections are mutually authenticated with a per-machine Ed25519 key and
end-to-end encrypted, so relays carry nothing they can read. See [iroh's
Security & Privacy](https://docs.iroh.computer/concepts/security-privacy).

Pairing is the only moment a connection from an unknown machine is
accepted, and a one-time secret carried in the ticket is what authorizes
it.

The history is stored in `~/.local/state/yank`, owner-readable, in plain
text, as clipboard managers generally do. Entries marked secret are the
exception: never written to disk on any machine, wiped from memory when
dropped, and never shown in the history.

## License

[WTFPL](LICENSE).
