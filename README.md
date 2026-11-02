# pocketeer

Manage an Analogue Pocket from your computer: cores, ROMs, saves and save
states. One binary with a TUI and a CLI.

```sh
cargo install --path crates/pocketeer
pocketeer            # the TUI
pocketeer docs       # the full CLI reference
```

## What it does

- **Cores**: install, update and remove openFPGA cores; see which BIOS files
  each one still needs, and move files left at the card root into place.
- **Games**: keep ROMs in a library on your computer. Imports are checked
  against No-Intro/Redump DATs, can take standard names, and get box art from
  libretro-thumbnails. You choose which games to copy to the card.
- **Saves**: back up the card's saves into the library, keeping every older
  version. Each save belongs to its game, so you can put an older version
  back on the card from the game itself.
- **Memories**: back up, restore and remove the save states made from the
  Pocket's menu. Each one shows the core and ROM it was made with.
- **Card**: free space, firmware, and macOS junk cleanup.

Games and saves removed from the library go to `.trash/` inside it, and a
save deleted from the card is backed up first. Every change is shown as a plan
before it happens.

## The TUI

Five tabs: Games, Memories, Cores, BIOS and Card. Each list has a text filter
(`/`) and a "show" filter (`l`); `?` lists every key, and the mouse works.

The Games tab shows the library and the card together. A game's saves are
one key away (`Enter`), with the copy on the card, the backup and the older
versions. Without a card the library still works, and a card is picked up as
soon as it is plugged in. Covers render as images in kitty, iTerm2 and sixel
terminals.

## The CLI

Every TUI action is also a command. `--dry-run` shows the plan and stops;
`--json` prints one stable object per run, for scripts and agents. A short
tour is in [docs/cli/](docs/cli/README.md), one file per area.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

## License

AGPL-3.0-or-later. Parts are adapted from
[Pocket Sync](https://github.com/neil-morrison44/pocket-sync) by Neil Morrison
(AGPL-3.0); adapted files say so in their headers. Not affiliated with Analogue.
