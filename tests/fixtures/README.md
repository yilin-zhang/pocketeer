# Test fixtures

Synthetic SD card layouts. Nothing here comes from a real card.

`sd-basic/`
- `agg23.Arduboy`: valid core.json, sample taken from Pocket Sync's test suite (AGPL-3.0)
- `spiritualized.GBA`: valid minimal core.json saved with a UTF-8 BOM
- `broken.Core`: truncated core.json → `PARSE_ERROR`
- `empty.Core`: no core.json → `CORE_JSON_MISSING`
- `Cores/._agg23.Arduboy`: macOS AppleDouble file, must be ignored
- `spiritualized.GBA/data.json`: requires `gba_bios.bin` (md5 of the string `GBA BIOS`)
- `agg23.Arduboy/data.json` + `Assets/arduboy/agg23.Arduboy/game1.json`: instance JSON wanting `games/game1.hex`; `_alternatives/` must be ignored
- `Cores/.DS_Store`: junk for `clean`

`core-gba-1.3.0.json`: the minimal spiritualized.GBA core.json that the
core and CLI tests both build cards and core zips from.

`memories/cart_1.sta`: a Memory made while playing a cartridge, whose
header names the game `KIRB`.

`covers/3x2.png`: a valid 3×2 PNG for `covers import`.
