# Saves and Memories

Cartridge saves live under `Saves/` (and `GB Studio/Saves/`); Memories are the
save states made from the Pocket's menu, under `Memories/`. Both are named by
their card path, and the library keeps every older version.

```sh
pocketeer saves backup                           # AP → computer; unresolved conflicts exit 1
pocketeer saves backup --no-states               # cartridge saves only, no Memories
pocketeer saves backup --conflicts ap             # explicitly choose AP for all conflicts
pocketeer saves backup --conflicts computer       # explicitly restore computer versions
pocketeer saves backup --conflicts skip           # leave conflicting pairs unchanged
pocketeer saves versions "Saves/gba/common/Game.sav"
pocketeer saves restore "Saves/gba/common/Game.sav" --version 12
pocketeer saves trash "Saves/gba/common/Game.sav"             # backup + versions → library .trash/
pocketeer saves trash "Saves/gba/common/Game.sav" --version 12
pocketeer saves trash "Saves/gba/common/Game.sav" --card      # delete from AP, archived first
pocketeer memories list
pocketeer memories backup
pocketeer memories restore "Memories/Save States/Spiritualized.GBA/Game_20251030.sta"
pocketeer memories trash "Memories/Save States/…/Game_20251030.sta" [--card]
```

A newer computer file or different bytes with ambiguous timestamps needs a
choice. Nonconflicting backups may proceed while conflicts remain untouched.
Every overwritten save is archived first. `restore` without `--version` uses
the current computer backup; with it, a specific history entry. Local-only
backups are retained, never automatically pushed to AP.

`trash` never erases: library copies move to `.trash/<time>/` inside the
library, and a save deleted from AP is copied into its history first unless the
library already has those bytes. A Memory moves together with the files of the
same name beside it. `memories list` reads each state's header: the core, its
version and the ROM file it was made with.
