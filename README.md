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

## License

AGPL-3.0-or-later. Parts are adapted from
[Pocket Sync](https://github.com/neil-morrison44/pocket-sync) by Neil Morrison
(AGPL-3.0); adapted files say so in their headers. Not affiliated with Analogue.
