# ROM identity and card reads

The card describes what is present, not what a subscription says should be
present. A path identifies a copy; platform + complete SHA-1 identifies its
content. Names are display and file-management choices. Multiple paths with
identical content remain distinct copies.

The disposable `.pocketeer/rom-index.msgpack` cache uses MessagePack tuples,
relative paths, file size and nanosecond modification time, binary SHA-1 and
binary sample digest. Version 1 includes the sampling layout. A missing or
unreadable cache is rebuilt. Directory scanning must finish successfully before
old entries can be pruned. Persistence uses a temporary file and rename, with a
writer lock. Cache write errors are separate from ROM operation results.

`card list` reads the directory tree and cache, not ROM contents. `card check`
waits for verification: cached files are sampled, new or changed files are fully
hashed. `card check --full` reads every byte. CLI stdout remains one JSON envelope
with `--json`; progress goes to stderr. File failures retain a visible error and
unknown identity instead of silently becoming card-only files.

Confidence states are `pending`, `cached`, `sampled`, `full`, and `error`. A
sampled match is not proof that every byte is unchanged. Full verification is
available when that distinction matters. An unresolved card file of matching
platform and size prevents declaring the corresponding local ROM local-only.

## Read-only USB experiment, 2026-11-01

On the user's mounted AP card, eight candidate ROMs totaled 69,271,552 bytes.
The directory walk took 0.0095 seconds. Seven files of at least 1 MiB were read
with macOS `F_NOCACHE` requested:

| Sampling per file | Total bytes (7 files) | Total time | Median per file |
| --- | ---: | ---: | ---: |
| Three 4 KiB ranges | 86,016 | 0.695 s | 99 ms |
| Three 16 KiB ranges | 344,064 | 0.689 s | 98 ms |
| Three 64 KiB ranges | 1,376,256 | 1.993 s | 279 ms |

Two complete 16 MiB reads took 23.789 and 23.553 seconds (about 0.68 MiB/s).
These are observations of this connection, not general hardware guarantees.
Version 1 samples 16 KiB at the start, middle and end; equal offsets collapse
for short files. Samples include file size and offsets in their digest.

OpenFPGA allows nested asset folders and mirrors them in Saves. See the
[official folder specification](https://www.analogue.co/developer/docs/openfpga/directories-and-sd-folder-structure).

## Transfers, renames and save paths

The TUI keeps a local title and marks a name difference with `≠`; details list
all matched AP paths. Rename always selects a side and exact copy, then shows
a plan. Local renames never queue work for an absent card. AP renames preserve
extensions, move ordinary `.sav`/`.rtc` companions in the mirrored save folder,
and update the cached path without rereading the ROM. Ambiguous shared save
stems are rejected. Memories stay untouched, with an explicit preview warning.

Save backups and history are keyed by the observed card-relative path, not a
registered device. An AP rename leaves old backup/history paths intact; the next
backup records the new path. Different cards using the same save path are compared
with the current backup and follow the same conflict rules. There is no automatic
per-card history namespace. Existing legacy database tables remain readable but
registration/subscription records no longer drive any operation.

GB Studio lists only top-level `.pocket` files and follows `GB Studio/Saves` for
ordinary save companions. Hidden files and symlinks are excluded. General Assets
files are ROM candidates unless declared by a core or recognized as sidecar data;
a scan cannot determine every core's firmware-specific file picker behavior.

Imports hash while copying and seed the card cache when the source metadata is
unchanged. Library→AP copies likewise hash during transfer and record the new
cache entry. Full background checks checkpoint each completed identity; clean
sample checks do not rewrite the cache. A foreground action cancels verification
at a chunk boundary and the worker resumes after the dialog/action finishes.
