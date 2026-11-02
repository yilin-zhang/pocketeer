# Library

```sh
pocketeer library init                           # ~/Pocketeer Library
pocketeer config set library <dir>               # somewhere else, persistently
pocketeer library import <folder|rom|zip>       # platform from each file's extension
pocketeer library import <folder> --platform gba [--dat gba.dat]
pocketeer library list [--platform gba] [query]
pocketeer library rename "Game (USA)" "Game (Europe)"
pocketeer library standardize                    # GoodTools-style names → No-Intro names
pocketeer library remove "Game (USA)"            # → .trash/ inside the library
```

With a DAT, hash matches take the DAT's name and count as verified. Without
one the file name is kept; `--standard-names` imports under the No-Intro name
when one matches. A library rename changes only the local file and title.
Card names are independent.
Imports always copy into the managed library. Without `--platform`, files whose
extension doesn't name a platform (`.bin`, say) are listed and left out.

## Identification

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

## Covers

```sh
pocketeer covers import "Game.gba" /path/to/cover.png [--dry-run]
pocketeer covers fetch [--platform gba]
pocketeer covers fetch --platform xyz --libretro-system "Vendor - System"
```

Local cover imports copy validated PNG/JPEG/WebP images into the library. The source
is kept, ROM renames preserve the association, and automatic downloads do not
overwrite imported covers. In the TUI, select a library game and press `c`;
enter an image path (Tab completes paths), then confirm the import or replacement.
