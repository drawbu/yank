# Changelog

All notable changes to yank are documented here.

## Unreleased

## 0.2.0

The clipboard is no longer a replicated history: every machine holds the
latest copy made anywhere on the mesh, and nothing else.

### Changed

- The sync protocol is `yank/sync/3`; machines running 0.1 cannot reach 0.2
  ones. Upgrade every machine.
- `max-entry-size` is now `max-size` in `config.toml`.
- `yank copy` no longer prints an entry label, and `yank paste` and
  `yank get` always act on the current clipboard.
- A copied folder is fetched in one request per machine rather than one per
  file.
- The daemon no longer writes colour codes to the journal.

### Removed

- `yank list`, `yank pick`, `yank rm` and `yank clear --history`.
- The `history-limit`, `history-budget` and `file-budget` settings. A
  `config.toml` still setting them is refused, and the daemon falls back to
  the defaults.
- `service.toml`: `yank service install` asks systemd where the unit comes
  from instead, and refuses one it did not write. Delete
  `~/.config/yank/service.toml` after upgrading.
- The history directory. Delete `~/.local/state/yank/history` and
  `~/.local/state/yank/clip.json` after upgrading: they hold the old history
  in plain text.

## 0.1.0 - 2026-09-05

### Added

- Initial release.
