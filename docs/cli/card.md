# Card

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

How identities are cached and when ROM files are read: [../card-index.md](../card-index.md).
