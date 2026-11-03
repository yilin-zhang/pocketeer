## Conventions

### Output

- `--json` (global): stdout carries exactly one object,
  `{"schema_version": N, "ok": bool, "data"?: …, "error"?: {code, message, remedy?, hint?, details?}}`.
  Progress and logs never go to stdout. `error.code` values are stable (table at the end).
  `remedy` names the obvious next step (`insert_card`, `find_rom`, `replan`, …); `hint` says
  it in words for the command line.
- Errors and warnings inside `data` (per-item failures, `warnings`, `errors`) have the same
  shape as `error`, without `hint`: branch on `code` and `remedy`.
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
  carry `items[]`, each with `target` (what was named) and `state`: `unplanned` (with
  `error`), `planned` (`plan`), `applied` (`plan`, `result`) or `failed` (`plan`, `error`).
- `removed` counts files or ROMs taken off the card or out of the library; `deleted` counts
  files explicitly deleted. Names asked for that don't exist
  are listed under `not_found` and count as failures (`SOME_FAILED`).
- `SOME_FAILED.details` has `failed` and `total` (failed plus planned changes).
- Operations are idempotent: installing an installed version plans no actions and changes nothing.

### Library and card

- The library is the backup; the card is just a card. Files pocketeer removes or replaces on
  the card are deleted (a failed operation is rolled back first). Files removed from the
  library go to `.trash/<timestamp>/` inside it; a save about to be overwritten on the card
  is copied into the library's save history first.
- The default game title is the local ROM filename. Local and AP names are independent;
  matching uses platform + complete SHA-1. A differing name is marked `≠` in the TUI.
  Local rename only changes the library; card rename takes an exact relative path and
  previews ordinary save companions. Memories are preserved with a warning.
- `card list` reads metadata and the compact card cache. `card check` samples cached files
  and hashes new/changed files; `--full` checks every byte. The TUI does this in a serial
  background worker. Sampled confidence is not a full-content guarantee.
- Files an installed core declares (BIOS and the like) are excluded from ROM matching and
  transfer selection. Nested Assets folders are supported; GB Studio uses root `.pocket` files.
- Imports always create managed copies. Card contents are observed, without registration,
  subscriptions, desired-state sync or offline rename replay.
- Save backup defaults AP → computer. Newer computer content or equal/ambiguous timestamps
  with differing bytes requires a choice; CLI leaves unresolved pairs alone and exits 1.
  `--conflicts ap|computer|skip` chooses explicitly. Restore archives AP before writing.
- Junk is scanned only on explicit `clean` / TUI Card `c`, then removal is previewed.
- Imports without a DAT look unverified names up in the libretro name list (No-Intro names,
  cached a week) and report a `suggested` standard name per file; `--standard-names` imports
  under it, `library standardize` renames existing games. A name match, not a DAT check.
- `library import` and `card import` fetch covers for new games afterwards when online
  (`--no-covers` to skip); `covers fetch` does it on demand.

### Settings

- Precedence: flag > environment variable > config file (`pocketeer config`) > default.
  Env vars: `POCKETEER_ROOT`, `POCKETEER_CACHE_DIR`, `POCKETEER_LIBRARY`, `POCKETEER_CONFIG`,
  `POCKETEER_GITHUB_TOKEN`. `POCKETEER_NO_IMAGES=1` makes the TUI draw covers with half-block
  cells instead of querying the terminal for an image protocol.
- `--offline` never touches the network (cached inventory and downloads only);
  `--refresh` revalidates the inventory even if fresh.
- The TUI (`pocketeer` with no subcommand) calls the same library functions as these commands;
  TUI actions have CLI equivalents. It runs without a card too: the
  Games tab works from the library alone.
