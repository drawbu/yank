# yank

`yank` replicates the clipboard across your machines over a peer-to-peer
private mesh (using [iroh]), with no server in between.

All the machines connected to your mesh share the same clipboard and recent
history.

- Clipboard is share *almost* instantly.
- Support pausing/resuming gracefully.
- Catches up when a machine is off, asleep or offline, and it comes back.
- Support copying files
- Support for sharing secret with a lifetime (like copying from a password manager).

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

A `yank` mesh is meant only for machines you own and control. It gives access
and read though your clipboard at all time, so be carefull.

Sway, niri, Hyprland, KDE and most others Wayland compositors are supported.
Tho, there is no X11, Gnome, or macOS support (more on that later). You can even
add a machine with no graphical session to take part to replicate your history
even when host are not online at the same time, and to use the CLI.

<details>
  <summary> Missing support (macOS, Gnome, and X11) </summary>

  The project is still super early, and don't particularly need the support
  for those, so I did not invest time on it. If there is some demand, and some
  testers available I would gladly add it to the project.

  Though, for Gnome some blockers exists. There is currently no interface a
  clipboard manager can use, as it does not implement the data-control
  protocol, and the [clipboard portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Clipboard.html)
  extends remote-desktop sessions rather than standing on its own. To be fair,
  these protocols let any client read every clipboard change without asking
  (like we do) (see [mutter#524](https://gitlab.gnome.org/GNOME/mutter/-/issues/524)). 
  We would have to create a GNOME Shell extension to be able to plug to it, so
  it is not on my roadmap right now.
</details>

## Installation

`yank` is not distributed by any package manager yet. With `cargo`:

```sh
$ cargo install --git https://tangled.org/drawbu.dev/yank.git --locked
# OR
$ cargo install --git https://github.com/drawbu/yank.git --locked
$ yank service install
```

With [Nix](https://nixos.org/), the flake exposes a package, as well as a Home
Manager module and NixOS module, that both add an enablable service. 

Home Manager:
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
> Do not run `yank service install` there, as it would shadow the nix service.

## Usage

To pair your machines, run this on one, and it will print you the command for
the second host. Reminder: a paired machine has access to everything you copy,
so be mindfull of which devices you give access. After that, the mesh is created
and nothing else is needed.

```sh
$ yank peer add
```

Configuration lives in `~/.config/yank/config.toml`, written with every
option commented out on first start. The daemon reads it once, so run

`yank service restart` after editing it.
Here are some useful commands. See `yank help`.

```sh
$ echo hi | yank copy       # copy from a pipe, like wl-copy
$ yank copy -f ./photos     # copy files, contents and all
$ yank paste                # print the clipboard, for pipes
$ yank get abc123-4         # write an entry's files here
$ yank list                 # the shared history, newest first
$ yank pick abc123-4        # put one of those back on the clipboard
$ yank rm abc123-4          # remove one entry from every machine
$ yank clear [--history]    # empty every machine's clipboard
$ yank copy --secret        # gone everywhere in 90 seconds
$ yank pause [--capture|--apply] [--for 30m]
$ yank resume
```

## Security and privacy

`yank` uses the superb crate [iroh], which establish direct connections between
endpoints, by find the best connection between them and maintaining them.
Connections are mutually authenticated with a per-machine Ed25519 key and
end-to-end encrypted.
See [iroh's Security & Privacy](https://docs.iroh.computer/concepts/security-privacy).

Non-secret history and copied files are stored in `~/.local/state/yank` in
plain.

## Contributing

Contributions and bug reports are welcome and appreciated. Feature requests are
welcome too, but may not be accepted: this project follows its maintainer's
direction, and it cannot maintain every possible feature.

AI-generated code is welcome, but you must disclose it. You remain responsible
for everything you submit. Undisclosed AI use or AI-written prose is slop and
will be discarded immediately.

## Acknowledgements

[jj-mesh](https://github.com/baptiste0928/jj-mesh) gave me the idea of this
project through it use of `iroh`, and help to shape the direction.

## License

[WTFPL](LICENSE).

[iroh]: https://www.iroh.computer/
