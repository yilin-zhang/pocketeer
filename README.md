# Pocketeer

[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
![Rust 1.99+](https://img.shields.io/badge/rust-1.99%2B-orange)

Pocketeer is a terminal game manager for the [Analogue Pocket](https://www.analogue.co/pocket).

It keeps your games, saves and covers in a library on your computer and copies them to and from the Pocket. It can also install cores and tell you which BIOS files they need.

## Install

You need Rust 1.99 or newer.

```sh
cargo install --git https://github.com/yilin-zhang/pocketeer
```

## Getting started

Put the Pocket's SD card in a reader, or connect the Pocket over USB, then run:

```sh
pocketeer
```

This opens the terminal UI. Pocketeer finds the mounted device by itself. The library is the folder `~/Pocketeer Library`, which is created the first time.

Press `?` to see every key. The keys on the Games page follow one rule: lowercase changes the library, and Shift changes the device.

| Key | In the library | Key | On the device |
|---|---|---|---|
| `i` | Import a game from a file | | |
| `p` | Pull a game from the device | `P` | Push a game to the device |
| `s` | Back up a save | | |
| `n` | Rename | `N` | Rename |
| `x` | Move to the system's trash | `X` | Delete for good |

Press Enter on a game to see the versions of its save. There, `S` pushes the selected version to the device.

## Command line

Everything the terminal UI does is also a command. Add `--json` to any of them for output that programs can read.

```sh
pocketeer games                       # list every game
pocketeer import game ~/Downloads/gba # import games from a folder
pocketeer push game gba/Tetris.gba    # copy a game to the device
pocketeer pull save                   # back up the device's saves
pocketeer cores list                  # list the cores
pocketeer status                      # show the device's firmware and free space
```

See `pocketeer --help` for the rest.

## Supported platforms

| Platform | Supported Cores |
|---|---|
| Game Boy | `Spiritualized.GB`, `budude2.GB` |
| Game Boy Color | `Spiritualized.GBC`, `budude2.GBC` |
| Game Boy Advance | `Spiritualized.GBA`, `K3V.GBA`, `mincer_ray.GBA` |
| Game Gear | `Spiritualized.GG`, `drizzt.GG` |
| Master System | `Spiritualized.SMS`, `drizzt.SMS` |
| NES | None yet |
| SNES | `agg23.SNES` |
| Genesis | `Spiritualized.Genesis`, `ericlewis.Genesis`, `drizzt.MegaDrive` |
| PC Engine | `agg23.PC Engine` |
| Neo Geo | None yet |

## License

[AGPL-3.0-or-later](LICENSE)
