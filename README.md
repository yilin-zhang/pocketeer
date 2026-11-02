# pocketeer

Manage an Analogue Pocket's cores, ROMs and saves. ROMs live in a library on
your computer; copy selected ROMs to the card and back up saves. TUI and CLI
in one binary.

```sh
cargo install --path crates/pocketeer
pocketeer            # the TUI
pocketeer docs       # the full CLI reference
```

## What it does

- **Cores**: browse the openFPGA inventory, install, update and remove cores;
  see which BIOS files each core still needs and move the ones you dropped at
  the card root into place.
- **Library**: import ROMs (verified against No-Intro/Redump DATs when you
  have one), rename to standard names, fetch box art from libretro-thumbnails.
  Removals go to a `.trash/` folder, never straight to deletion.
- **Card**: discover actual files, including nested ROM folders. Match local and
  card copies by content hash, even when names differ. A compact on-card cache
  makes reconnects cheap; the TUI verifies in the background.
- **Transfers**: explicitly copy selected ROMs or import card files. Imports
  always make managed copies. No registration, subscriptions or queued renames.
- **Saves**: back up AP → computer with history. A newer computer save or
  ambiguous timestamp produces a conflict; restoring to AP is always explicit.
- **Cleanup**: scan for macOS junk only when requested, then preview removal.
- Works without a card: the Games tab manages the library on its own, and a
  card that is plugged in or pulled is noticed by itself.

## The TUI

Four tabs, each a list with a text filter (`/`) and a show filter (`l`):
**Cores**, **BIOS**, **Games** (library and card together; a WHERE column says
which) and **Card**. `?` lists the keys; the mouse works. File changes show
their plan with Apply and Cancel buttons first. Background verification only
updates the disposable cache. A small `≠` marks matching ROMs with different
names; `e` offers explicit local/AP rename choices. Covers render as real images in
kitty, iTerm2 and sixel terminals and as half-block pixels elsewhere.

## The CLI

Every TUI action is also a command. Each prints its plan before acting,
`--dry-run` stops after the plan, and `--json` gives one stable object per
run. The short tour is in [docs/cli.md](docs/cli.md); `pocketeer docs` prints
the complete generated reference, written for agents as much as for people.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

### Code organization

`pocketeer-core` owns behavior; `pocketeer` is the CLI and the TUI on top of it.
Core is four layers, and a lower layer never imports a higher one:

```text
fsutil/  net.rs  config.rs  error.rs  plan.rs   files, HTTP, settings, errors, the plan/apply contract
catalog/                                        external knowledge: core inventory, DAT files, libretro sources
card/     library/                              the SD card; the local library (neither imports the other)
transfer/                                       card ⇄ library: matching, copies both ways, saves, change detection
```

Inside each layer one file is one concept: `card/mod.rs` has the card layout and
detection, `card/roms.rs` the ROM scan and identity cache, `card/install/` the
core installer, `library/db.rs` every SQL statement, `library/import.rs` the
importer, and so on. Each write operation is a `plan_*` function returning plain
data and an `apply_*` function taking it; `plan.rs` holds the two traits the
front ends need from them.

The binary is `ops/` (the use cases, shared by CLI and TUI, taking plain request
structs), `text/` (human-readable renderings, one file per `ops` module),
`cli.rs` (clap definitions and their conversion into requests), `main.rs`
(dispatch) and `tui/` (`worker.rs` runs ops on background threads, `pending.rs`
is one operation = one variant with its plan, preview and apply, `app/` is the
state and keys, `ui/` draws).

## License

AGPL-3.0-or-later. Parts are adapted from
[Pocket Sync](https://github.com/neil-morrison44/pocket-sync) by Neil Morrison
(AGPL-3.0); adapted files say so in their headers. Not affiliated with Analogue.
