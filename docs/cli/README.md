# CLI

Everything the TUI does is also a command. Changes print their plan and
apply it; `--dry-run` prints the plan and stops. `--json` gives one object per
run. `pocketeer docs` prints the full generated reference; `pocketeer docs
saves` prints only one command group.

One file per area:

- [cores.md](cores.md): status, cores, the inventory, BIOS files, junk cleanup
- [library.md](library.md): the ROM library, identification, covers
- [card.md](card.md): ROMs on the card, copies both ways
- [saves.md](saves.md): saves and Memories, backups, history, trash

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
