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
when one matches. A library rename changes only the local file and title.
Card names are independent.
Imports always copy into the managed library.

## Card

```sh
pocketeer card list                              # metadata + cached identities, no ROM reads
pocketeer card check                             # sample cached files; fully hash new/changed files
pocketeer card check --full                      # explicit full verification
pocketeer card copy "Game.gba"                   # copy selected local ROM, no subscriptions
pocketeer card import "Assets/gba/common/sub/Game.gba"
pocketeer card rename "Assets/gba/common/sub/Game.gba" "New name"
```

Use exact card-relative paths for import/rename. Same platform + hash matches
content regardless of name; duplicate copies retain distinct paths. Pending or
failed checks stay unknown. Copies/imports preserve existing destination files.
A card rename previews the ROM and its ordinary save companions; save states in
Memories stay unchanged because their association is not reliably established.

## Saves and covers

```sh
pocketeer saves backup                           # AP → computer; unresolved conflicts exit 1
pocketeer saves backup --conflicts ap             # explicitly choose AP for all conflicts
pocketeer saves backup --conflicts computer       # explicitly restore computer versions
pocketeer saves backup --conflicts skip           # leave conflicting pairs unchanged
pocketeer saves versions "Saves/gba/common/Game.sav"
pocketeer saves restore "Saves/gba/common/Game.sav" --version 12
pocketeer covers import "Game.gba" /path/to/cover.png [--dry-run]
pocketeer covers fetch [--platform gba]
pocketeer covers fetch --platform xyz --libretro-system "Vendor - System"
```

Local cover imports copy validated PNG/JPEG/WebP images into the library. The source
is kept, ROM renames preserve the association, and automatic downloads do not
overwrite imported covers. In the TUI, select a library game and press `b`;
enter an image path (Tab completes paths), then confirm the import or replacement.

A newer computer file or different bytes with ambiguous timestamps needs a
choice. Nonconflicting backups may proceed while conflicts remain untouched.
Every overwritten save is archived first. `restore` without `--version` uses
the current computer backup; with it, a specific history entry. Local-only
backups are retained, never automatically pushed to AP.

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

## Automatic ROM identification

`pocketeer library identify [--platform gba]` fetches the platform DAT from
[libretro-database's No-Intro collection](https://github.com/libretro/libretro-database/tree/master/metadat/no-intro) and matches stored ROM SHA-1 + size.
`--offline` uses cached catalogs; `--refresh` revalidates them immediately.
Catalogs normally revalidate after seven days, using ETags. Results are stored
per ROM: unchecked, matched, no_match, error, ambiguous or unsupported, with
catalog URL, content revision, check time and any retry error. A no_match is
cached until the catalog changes; fetch/parse failures remain retryable and
never erase established results. JSON library listings include these fields.

TUI startup and imports identify automatically in the background; `d` retries.
Identification keeps filenames and user titles. The matched standard title
selects cover art; `t` / `library standardize --dry-run` previews explicit
renames using the DAT's ROM filename. Imported covers remain protected.
Unsupported platforms and files whose whole-file hash differs (including
headered or modified ROMs) are not guessed from their names.
