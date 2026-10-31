## Conventions

### Output

- `--json` (global): stdout carries exactly one object,
  `{"schema_version": N, "ok": bool, "data"?: …, "error"?: {code, message, hint?, details?}}`.
  Progress and logs never go to stdout. `error.code` values are stable (table at the end).
  A wrong command line or `--help` still yields the envelope: clap's text goes to stderr,
  the error code is `USAGE` (exit 2).
- Exit codes: `0` success, `1` error, `2` usage error.

### Plans

- Every command that changes files prints its plan, then applies it. `--dry-run` prints the
  plan and stops. Agents: run with `--dry-run` first when the outcome matters, read `data`,
  then run for real.
- One shape for all plans: `data.mode` is `dry_run` or `applied`; the plan's own
  fields follow; `data.result` appears only after an apply and holds what was done (counts
  plus a `failed` list where items can fail one by one). Batches (`cores install|update|remove`)
  carry `outcomes[]`, each with `plan?`, `result?`, `error?`.
- `removed` counts files or ROMs taken off the card or out of the library; `deleted` counts
  files a sync deleted because the source no longer has them. Names asked for that don't exist
  are listed under `not_found` and count as failures (`SOME_FAILED`).
- `SOME_FAILED.details` has `failed` and `total` (failed plus planned changes).
- Operations are idempotent: installing an installed version reports `changed: false`.

### Library and card

- The library is the backup; the card is just a card. Files pocketeer removes or replaces on
  the card are deleted (a failed operation is rolled back first). Files removed from the
  library go to `.trash/<timestamp>/` inside it; a save about to be overwritten on the card
  is copied into the library's save history first.
- A library ROM's display name is its file name. `library rename` moves the file, the game,
  the mounted card's copy and that copy's saves and save states together. Without a card the
  rename is remembered and the next `device sync` applies it to a card still carrying the
  old name, saves included.
- Nothing on a card is hashed unless `--verify` is given: card reads over USB are slow, so
  orphan detection and sync compare names and sizes only.
- Files an installed core declares in its data.json (BIOS and the like) are never treated as
  ROMs: `device orphans` and `device import` skip them, sync never renames or deletes them.
- Imports without a DAT look unverified names up in the libretro name list (No-Intro names,
  cached a week) and report a `suggested` standard name per file; `--standard-names` imports
  under it, `library standardize` renames existing games. A name match, not a DAT check.
- `library import` and `device import` fetch covers for new games afterwards when online
  (`--no-covers` to skip); `covers fetch` does it on demand.

### Settings

- Precedence: flag > environment variable > config file (`pocketeer config`) > default.
  Env vars: `POCKETEER_ROOT`, `POCKETEER_CACHE_DIR`, `POCKETEER_LIBRARY`, `POCKETEER_CONFIG`,
  `POCKETEER_GITHUB_TOKEN`. `POCKETEER_NO_IMAGES=1` makes the TUI draw covers with half-block
  cells instead of querying the terminal for an image protocol.
- `--offline` never touches the network (cached inventory and downloads only);
  `--refresh` revalidates the inventory even if fresh.
- The TUI (`pocketeer` with no subcommand) calls the same library functions as these commands;
  everything it can do has a CLI equivalent, and vice versa. It runs without a card too: the
  Games tab works from the library alone.
