//! Command-line definitions (clap). Behaviour lives in `commands/`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Manage cores, ROMs and saves for an Analogue Pocket: a local library, the
/// cards it syncs to, and the SD card itself.
///
/// Run without a subcommand to open the TUI.
/// With --json, every command prints exactly one JSON object to stdout:
/// {"schema_version":1,"ok":true,"data":...} or {"schema_version":1,"ok":false,"error":{...}}.
/// Exit codes: 0 success, 1 error, 2 usage error.
/// Every command that changes something, on the card or in the library alone
/// (library import/rename/remove/standardize, saves restore), prints its plan
/// and applies it; --dry-run prints the plan only.
#[derive(Debug, Parser)]
#[command(name = "pocketeer", version, propagate_version = true)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Args, Clone)]
pub struct Global {
    /// Print machine-readable JSON to stdout instead of human output.
    #[arg(long, global = true)]
    pub json: bool,

    /// Pocket SD card root. Auto-detected among mounted volumes if omitted.
    #[arg(long, global = true, value_name = "PATH", env = "POCKETEER_ROOT")]
    pub root: Option<PathBuf>,

    /// Never use the network; rely on cached inventory data.
    #[arg(long, global = true)]
    pub offline: bool,

    /// Revalidate cached inventory data even if it is fresh.
    #[arg(long, global = true, conflicts_with = "offline")]
    pub refresh: bool,

    /// Directory for cached downloads (default: the OS cache dir, e.g. ~/Library/Caches/pocketeer).
    #[arg(long, global = true, value_name = "DIR", env = "POCKETEER_CACHE_DIR")]
    pub cache_dir: Option<PathBuf>,

    /// Library folder (default: config `library`, else ~/Pocketeer Library).
    #[arg(long, global = true, value_name = "DIR", env = "POCKETEER_LIBRARY")]
    pub library: Option<PathBuf>,

    /// Config file (default: the OS config dir, e.g. ~/Library/Application Support/pocketeer/config.toml).
    #[arg(long, global = true, value_name = "FILE", env = "POCKETEER_CONFIG")]
    pub config: Option<PathBuf>,

    /// GitHub token, used only for github.com requests (avoids rate limits).
    #[arg(
        long,
        global = true,
        value_name = "TOKEN",
        env = "POCKETEER_GITHUB_TOKEN",
        hide_env_values = true
    )]
    pub github_token: Option<String>,
}

/// Shared by every command that changes the card or the library.
#[derive(Debug, Args, Clone, Copy, Default)]
pub struct ApplyArgs {
    /// Show the plan and exit without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show SD card, firmware, disk space and core counts.
    Status,
    /// Work with cores.
    #[command(subcommand)]
    Cores(CoresCommand),
    /// Work with the community core inventory.
    #[command(subcommand)]
    Inventory(InventoryCommand),
    /// BIOS and other files cores need.
    #[command(subcommand)]
    Bios(BiosCommand),
    /// The local library: the source of truth your cards are synced from.
    #[command(subcommand)]
    Library(LibraryCommand),
    /// Inspect and manage the mounted card by content identity.
    #[command(subcommand)]
    Card(CardCommand),
    /// Save files and save states: card ⇄ library, with history.
    #[command(subcommand)]
    Saves(SavesCommand),
    /// Box art for library games (from libretro-thumbnails).
    #[command(subcommand)]
    Covers(CoversCommand),
    /// Persistent settings (library path, cache dir, card root, GitHub token).
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Print the full CLI reference as Markdown (for people and agents).
    Docs {
        /// Only this command and its subcommands, e.g. `cores install`.
        #[arg(value_name = "COMMAND")]
        path: Vec<String>,
    },
    /// Remove macOS junk (._* files, .DS_Store, __MACOSX) from the card.
    Clean {
        /// Also remove .Spotlight-V100, .fseventsd, .Trashes, .TemporaryItems.
        #[arg(long)]
        system: bool,
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

#[derive(Debug, Subcommand)]
pub enum CardCommand {
    /// Rename one exact card ROM path and its mirrored save files.
    Rename {
        /// Exact ROM path relative to the card root, including nested folders.
        path: PathBuf,
        /// New filename; omit the extension to keep it.
        new_name: String,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Copy selected local ROMs to this card; show the plan first with --dry-run.
    Copy {
        /// Library ROM IDs, filenames or game names.
        #[arg(required = true, value_name = "ROM")]
        roms: Vec<String>,
        /// Disambiguate names shared by several platforms.
        #[arg(long)]
        platform: Option<String>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Copy selected card ROM paths into the library (default: all unmatched files).
    Import(CardImportArgs),
    /// List ROMs using cached hashes, without reading ROM contents.
    List,
    /// Update unknown/changed ROM hashes and sample-check cached files; waits until finished.
    Check {
        /// Read every byte, even when metadata and samples match.
        #[arg(long)]
        full: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum CoresCommand {
    /// List installed cores, with update status when the inventory is available.
    List {
        /// Only cores with an update available.
        #[arg(long)]
        outdated: bool,
    },
    /// Search the inventory for cores to install.
    Search {
        /// Case-insensitive substring of the core id (e.g. "gba", "jotego").
        query: Option<String>,
        /// Only cores for this platform id (e.g. "gba", "snes").
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        /// Include cores that are already installed.
        #[arg(long, alias = "all")]
        installed: bool,
    },
    /// Show everything known about one core.
    Info {
        /// Core id, `<author>.<shortname>` (case-insensitive).
        id: String,
    },
    /// Install cores from the inventory (or a local zip).
    Install(InstallArgs),
    /// Update installed cores to the latest inventory release.
    Update {
        /// Core ids to update. Omit with --all to update everything outdated.
        #[arg(value_name = "ID", required_unless_present = "all")]
        ids: Vec<String>,
        /// Update every installed core that has an update.
        #[arg(long, conflicts_with = "ids")]
        all: bool,
        /// Don't overwrite existing Platforms/ files (keeps your customisations).
        #[arg(long)]
        keep_platforms: bool,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Remove installed cores (Cores/, Presets/, Settings/ are deleted).
    Remove {
        /// Core ids to remove.
        #[arg(value_name = "ID", required = true)]
        ids: Vec<String>,
        /// Also remove the core's Assets/ and Saves/ folders.
        #[arg(long)]
        purge: bool,
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

#[derive(Debug, Args, Clone)]
pub struct InstallArgs {
    /// Core ids, optionally pinned: `jotego.jtcps1` or `jotego.jtcps1@1.2.0`.
    #[arg(value_name = "ID[@VERSION]", required_unless_present = "from")]
    pub specs: Vec<String>,
    /// Install from a release zip on disk instead of downloading.
    #[arg(long, value_name = "FILE", conflicts_with = "specs")]
    pub from: Option<PathBuf>,
    /// Reinstall even if this version is already installed.
    #[arg(long)]
    pub force: bool,
    /// Don't overwrite existing Platforms/ files (keeps your customisations).
    #[arg(long)]
    pub keep_platforms: bool,
    #[command(flatten)]
    pub apply: ApplyArgs,
}

#[derive(Debug, Subcommand)]
pub enum InventoryCommand {
    /// Fetch the latest inventory now and report its size.
    Refresh,
}

#[derive(Debug, Subcommand)]
pub enum BiosCommand {
    /// Show BIOS and data files cores need, and which are missing.
    Check {
        /// Only these cores (default: all installed).
        #[arg(value_name = "ID")]
        ids: Vec<String>,
        /// Also list files that are present.
        #[arg(long, alias = "all")]
        present: bool,
    },
    /// Move files dropped at the SD card root into the folders cores expect.
    Place {
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

#[derive(Debug, Subcommand)]
pub enum LibraryCommand {
    /// Identify ROMs by SHA-1 and size using automatically cached DATs.
    Identify {
        /// Only this platform.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
    },
    /// Create a new library (default: ~/Pocketeer Library, or --library).
    Init,
    /// Show where the library is and what's in it.
    Status,
    /// Import ROMs from a folder into the library (zips are extracted).
    Import(ImportArgs),
    /// List games in the library.
    List {
        /// Only this platform.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        /// Case-insensitive substring of the game name.
        query: Option<String>,
    },
    /// Rename a local ROM file and its game title; card copies keep their names.
    Rename {
        /// ROM id, file name or game name (case-insensitive).
        #[arg(value_name = "ROM")]
        rom: String,
        /// New file name; without an extension the old one is kept.
        #[arg(value_name = "NEW_NAME")]
        new_name: String,
        /// Disambiguate when the same name exists on several platforms.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Rename library games to the standard (No-Intro) names they match.
    Standardize {
        /// Only this platform.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Remove ROMs from the library (files go to .trash/ inside it; cards are untouched).
    Remove {
        /// ROM ids, file names or game names (a game name takes all its ROMs).
        #[arg(value_name = "ROM", required = true)]
        roms: Vec<String>,
        /// Disambiguate when the same name exists on several platforms.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

#[derive(Debug, Args, Clone)]
pub struct ImportArgs {
    /// Folder of ROMs on this computer (scanned recursively), or one ROM / zip.
    pub dir: PathBuf,
    /// Platform id these ROMs belong to, e.g. "gba".
    #[arg(long, value_name = "ID")]
    pub platform: String,
    /// Optional XML/clrmamepro DAT to verify and name files with.
    #[arg(long, value_name = "FILE")]
    pub dat: Option<PathBuf>,
    /// Skip files the DAT doesn't know (default: import them as unverified).
    #[arg(long, requires = "dat")]
    pub dat_only: bool,
    /// Don't fetch covers for the imported games afterwards.
    #[arg(long)]
    pub no_covers: bool,
    /// Import files under the standard (No-Intro) name they match, when a DAT
    /// didn't name them. Needs the libretro name list (network, cached a week).
    #[arg(long)]
    pub standard_names: bool,
    #[command(flatten)]
    pub apply: ApplyArgs,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show where the config file is.
    Path,
    /// Show all settings, or one.
    Get {
        /// One of: library, cache-dir, root, github-token (default: all).
        #[arg(value_name = "KEY")]
        key: Option<String>,
    },
    /// Set a value. Keys: library, cache-dir, root, github-token (stored in plain text).
    Set {
        /// Setting to change: library, cache-dir, root or github-token.
        #[arg(value_name = "KEY")]
        key: String,
        /// New value (a path for library, cache-dir and root).
        #[arg(value_name = "VALUE")]
        value: String,
    },
    /// Remove a value.
    Unset {
        /// Setting to clear: library, cache-dir, root or github-token.
        #[arg(value_name = "KEY")]
        key: String,
    },
}

#[derive(Debug, Args, Clone, Default)]
pub struct CardImportArgs {
    /// Only this platform.
    #[arg(long, value_name = "ID")]
    pub platform: Option<String>,
    /// Card-relative ROM paths (default: every unmatched file).
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
    /// Optional XML/clrmamepro DAT to verify and name the files with.
    #[arg(long, value_name = "FILE")]
    pub dat: Option<PathBuf>,
    /// Skip files the DAT doesn't know (default: import them as unverified).
    #[arg(long, requires = "dat")]
    pub dat_only: bool,
    /// Don't fetch covers for the imported games afterwards.
    #[arg(long)]
    pub no_covers: bool,
    /// Import files under the standard (No-Intro) name they match.
    #[arg(long)]
    pub standard_names: bool,
    #[command(flatten)]
    pub apply: ApplyArgs,
}

#[derive(Debug, Subcommand)]
pub enum SavesCommand {
    /// Back up AP saves. Newer computer copies and content conflicts need a choice.
    Backup {
        /// Leave Memories save states alone.
        #[arg(long)]
        no_states: bool,
        /// Explicitly resolve every conflict; omitted means leave conflicts unchanged.
        #[arg(long, value_enum)]
        conflicts: Option<SaveConflictArg>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Save files the library holds.
    List,
    /// Older copies of one save file.
    Versions {
        /// Path as on the card, e.g. "Saves/gba/common/Game.sav".
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
    /// Restore a computer backup to AP, archiving AP's current bytes first.
    #[command(disable_version_flag = true)]
    Restore {
        /// Card-relative save path.
        path: PathBuf,
        /// Use this historical version instead of the current backup.
        #[arg(long)]
        version: Option<i64>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SaveConflictArg {
    Ap,
    Computer,
    Skip,
}

#[derive(Debug, Subcommand)]
pub enum CoversCommand {
    /// Copy a local PNG/JPEG/WebP into the library as this game's cover.
    Import {
        /// ROM id, filename or game name identifying the game.
        #[arg(value_name = "ROM")]
        rom: String,
        /// Local PNG, JPEG or WebP image to copy; the source is kept.
        #[arg(value_name = "IMAGE")]
        image: PathBuf,
        /// Disambiguate names shared by several platforms.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Download missing covers (matched by DAT game name).
    Fetch {
        /// Only this platform.
        #[arg(long, value_name = "ID")]
        platform: Option<String>,
        /// Retry games that had no cover last time.
        #[arg(long)]
        retry: bool,
        /// libretro system name to use, e.g. "Nintendo - Game Boy Advance" (for unmapped platforms).
        #[arg(long, value_name = "NAME", alias = "system", requires = "platform")]
        libretro_system: Option<String>,
    },
    /// How many games have covers, per platform.
    Status,
}
