# CLI

Everything the TUI does is also a command. Changes print their plan and
apply it; `--dry-run` prints the plan and stops. `--json` gives one object per
run. `pocketeer docs [command]` prints the full generated reference.

## Cores

```sh
pocketeer status                                 # card, firmware, disk, core counts
pocketeer cores list [--outdated]
pocketeer cores search gba
pocketeer cores info jotego.jtcps1
pocketeer cores install jotego.jtcps1            # also name@version, or --from core.zip
pocketeer cores update --all
pocketeer cores remove jotego.jtcps1             # --purge removes settings too
pocketeer inventory refresh
```

## BIOS and junk

```sh
pocketeer bios check                             # what cores need, and what's missing
pocketeer bios place                             # files dropped at the SD root → where they belong
pocketeer clean                                  # ._* / .DS_Store / __MACOSX
```

## Library

```sh
pocketeer library init                           # ~/Pocketeer Library
pocketeer config set library <dir>               # somewhere else, persistently
pocketeer library import <folder|rom|zip> --platform gba [--dat gba.dat]
pocketeer library list [--platform gba] [query]
pocketeer library rename "Game (USA)" "Game (Europe)"
pocketeer library standardize                    # GoodTools-style names → No-Intro names
pocketeer library remove "Game (USA)"            # → .trash/ inside the library
```

With a DAT, hash matches take the DAT's name and count as verified. Without
one the file name is kept; `--standard-names` imports under the No-Intro name
when one matches. A rename moves file, game, card copy and saves together.

## Devices

```sh
pocketeer device add --name "Pocket A"           # register the mounted card
pocketeer device subscribe --platform gba --core spiritualized.GBA
pocketeer device sync                            # library → card: ROMs and cores
pocketeer device orphans                         # ROMs only on the card
pocketeer device import                          # … into the library
```

Sync is one way. Pending renames are replayed by name; nothing on the card is
hashed unless you pass `--verify`.

## Saves and covers

```sh
pocketeer saves sync                             # card ⇄ library, newest wins, history kept
pocketeer saves versions "Saves/gba/common/Game.sav"
pocketeer saves restore <ID>
pocketeer covers fetch [--platform gba]
pocketeer covers fetch --platform xyz --libretro-system "Vendor - System"
```

## Options

| | |
|---|---|
| `--root <path>` / `POCKETEER_ROOT` | the card, when auto-detection isn't enough |
| `--library <dir>` / `POCKETEER_LIBRARY` | the library |
| `--offline` / `--refresh` | never touch the network / revalidate the inventory |
| `--json` | one JSON object on stdout, exit 0 ok, 1 error, 2 usage |
| `--dry-run` | show the plan, change nothing |

Precedence: flag, environment variable, `pocketeer config`, default.

## JSON

```json
{ "schema_version": 1, "ok": true,  "data": { ... } }
{ "schema_version": 1, "ok": false, "error": { "code": "SD_NOT_FOUND", "message": "...", "hint": "...", "details": { ... } } }
```

Error codes are stable. With `--dry-run`, `data` is the plan; after a run it
also carries `result`.
