//! Command-line definitions (clap), their conversion into `ops` requests,
//! and the `docs` command, which renders this very command tree.

use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand};
use pocketeer_core::config::ConfigKey;
use pocketeer_core::{Error, Result, error::CODES, transfer::saves::ConflictChoice};
use serde::Serialize;

use crate::{ops, output::SCHEMA_VERSION};

/// Manage cores, ROMs and saves for an Analogue Pocket: a ROM library on
/// this computer, and the SD card you copy ROMs to and back up saves from.
///
/// Run without a subcommand to open the TUI.
/// With --json, every command prints exactly one JSON object to stdout:
/// {"schema_version":1,"ok":true,"data":...} or {"schema_version":1,"ok":false,"error":{...}}.
/// Exit codes: 0 success, 1 error, 2 usage error.
/// Every command that changes something, on the card or in the library alone
/// (library import/rename/remove/standardize, saves restore), prints its plan
/// and applies it; --dry-run prints the plan only.
// Every command and argument in this file needs a `///` doc comment:
// `pocketeer docs` and `--help` are generated from them.
#[derive(Debug, Parser)]
#[command(name = "pocketeer", version, propagate_version = true)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Args, Clone, Default)]
#[command(next_help_heading = "Global options")]
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

    /// Where libretro DATs are downloaded from (tests and mirrors).
    #[arg(long, global = true, hide = true, env = "POCKETEER_DAT_BASE")]
    pub dat_base: Option<String>,

    /// Where libretro thumbnails are downloaded from (tests and mirrors).
    #[arg(long, global = true, hide = true, env = "POCKETEER_THUMBNAILS_BASE")]
    pub thumbnails_base: Option<String>,
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
    /// The ROM library on this computer: import, identify, rename, remove.
    #[command(subcommand)]
    Library(LibraryCommand),
    /// Inspect and manage the mounted card by content identity.
    #[command(subcommand)]
    Card(CardCommand),
    /// Save files and save states: card ⇄ library, with history.
    #[command(subcommand)]
    Saves(SavesCommand),
    /// Memories (save states made from the Pocket's menu).
    #[command(subcommand)]
    Memories(MemoriesCommand),
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
    /// Platform id for every file, e.g. "gba". Without it each file's
    /// platform comes from its extension, and files it can't tell are left out.
    #[arg(long, value_name = "ID")]
    pub platform: Option<String>,
    #[command(flatten)]
    pub flags: ImportFlags,
    #[command(flatten)]
    pub apply: ApplyArgs,
}

/// How `library import` and `card import` treat the files they import.
#[derive(Debug, Args, Clone, Default)]
pub struct ImportFlags {
    /// XML/clrmamepro DAT to verify and name the files with (one platform's,
    /// so it needs --platform).
    #[arg(long, value_name = "FILE", requires = "platform")]
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
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show where the config file is.
    Path,
    /// Show all settings, or one.
    Get {
        /// The setting (default: all).
        #[arg(value_name = "KEY", value_parser = config_key())]
        key: Option<ConfigKey>,
    },
    /// Set a value (the GitHub token is stored in plain text).
    Set {
        /// The setting to change.
        #[arg(value_name = "KEY", value_parser = config_key())]
        key: ConfigKey,
        /// New value (a path for library, cache-dir and root).
        #[arg(value_name = "VALUE")]
        value: String,
    },
    /// Remove a value.
    Unset {
        /// The setting to clear.
        #[arg(value_name = "KEY", value_parser = config_key())]
        key: ConfigKey,
    },
}

/// Config keys by name; `--help` lists them.
fn config_key() -> impl clap::builder::TypedValueParser<Value = ConfigKey> {
    use clap::builder::TypedValueParser;
    clap::builder::PossibleValuesParser::new(ConfigKey::ALL.map(ConfigKey::name))
        .map(|name| ConfigKey::from_name(&name).expect("one of the listed names"))
}

#[derive(Debug, Args, Clone, Default)]
pub struct CardImportArgs {
    /// Only this platform.
    #[arg(long, value_name = "ID")]
    pub platform: Option<String>,
    /// Card-relative ROM paths (default: every unmatched file).
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
    #[command(flatten)]
    pub flags: ImportFlags,
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
    /// Move the library's copies of a save to its .trash/ folder, or delete
    /// it from AP (archived into the library first).
    #[command(disable_version_flag = true)]
    Trash {
        /// Card-relative save path.
        path: PathBuf,
        /// Delete the save from AP instead.
        #[arg(long, conflicts_with_all = ["version", "backup"])]
        card: bool,
        /// Only this older version.
        #[arg(long, conflicts_with = "backup")]
        version: Option<i64>,
        /// Only the current backup; older versions stay.
        #[arg(long)]
        backup: bool,
        #[command(flatten)]
        apply: ApplyArgs,
    },
}

#[derive(Debug, Subcommand)]
pub enum MemoriesCommand {
    /// Memories on AP and in the library.
    List,
    /// Back up new and changed Memories from AP.
    Backup {
        /// Explicitly resolve every conflict; omitted means leave conflicts unchanged.
        #[arg(long, value_enum)]
        conflicts: Option<SaveConflictArg>,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Put the library's copy of a Memory back on AP.
    Restore {
        /// Card-relative path of the .sta file.
        path: PathBuf,
        #[command(flatten)]
        apply: ApplyArgs,
    },
    /// Move a Memory to the library's .trash/, or delete it from AP
    /// (archived into the library first).
    Trash {
        /// Card-relative path of the .sta file.
        path: PathBuf,
        /// Delete it from AP instead.
        #[arg(long)]
        card: bool,
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

// ---------------------------------------------------------------------------
// Conversions into the plain request types `ops` takes.

impl From<ApplyArgs> for ops::Mode {
    fn from(args: ApplyArgs) -> Self {
        if args.dry_run {
            ops::Mode::DryRun
        } else {
            ops::Mode::Applied
        }
    }
}

impl From<ImportFlags> for ops::library::ImportSettings {
    fn from(f: ImportFlags) -> Self {
        ops::library::ImportSettings {
            dat: f.dat,
            dat_only: f.dat_only,
            standard_names: f.standard_names,
            covers: !f.no_covers,
        }
    }
}

impl From<CardImportArgs> for ops::card::ImportRequest {
    fn from(a: CardImportArgs) -> Self {
        ops::card::ImportRequest {
            platform: a.platform,
            paths: a.paths,
            settings: a.flags.into(),
        }
    }
}

impl From<InstallArgs> for ops::cores::InstallRequest {
    fn from(a: InstallArgs) -> Self {
        ops::cores::InstallRequest {
            specs: a.specs,
            from: a.from,
            force: a.force,
            keep_platforms: a.keep_platforms,
        }
    }
}

impl From<SaveConflictArg> for ConflictChoice {
    fn from(a: SaveConflictArg) -> Self {
        match a {
            SaveConflictArg::Ap => ConflictChoice::Card,
            SaveConflictArg::Computer => ConflictChoice::Computer,
            SaveConflictArg::Skip => ConflictChoice::Skip,
        }
    }
}

// ---------------------------------------------------------------------------
// `pocketeer docs`: the whole CLI reference, generated from the clap command
// tree so it can never drift from the real flags.

#[derive(Debug, Serialize)]
pub struct DocsReport {
    pub markdown: String,
    pub commands: Vec<String>,
    pub error_codes: Vec<ErrorCode>,
}

#[derive(Debug, Serialize)]
pub struct ErrorCode {
    pub code: &'static str,
    pub meaning: &'static str,
}

/// The hand-written part of the reference; the rest is generated from clap.
const CONVENTIONS: &str = include_str!("../docs/conventions.md");

pub fn docs(path: &[String]) -> Result<DocsReport> {
    let mut root = Cli::command();
    root.build();

    // Narrow to a subtree if asked.
    let mut target: &mut clap::Command = &mut root;
    let mut prefix = vec!["pocketeer".to_string()];
    for name in path {
        target = target
            .find_subcommand_mut(name)
            .ok_or_else(|| Error::Usage {
                message: format!("no command `{}`", path.join(" ")),
                hint: Some("run `pocketeer docs` for the full list".into()),
            })?;
        prefix.push(name.clone());
    }

    let mut md = String::new();
    let mut commands = Vec::new();
    if path.is_empty() {
        md.push_str(&format!(
            "# pocketeer CLI reference\n\nSchema version {SCHEMA_VERSION}. Generated by `pocketeer docs`.\n\n"
        ));
        md.push_str(CONVENTIONS);
        md.push_str("\n## Commands\n\n");
    } else {
        md.push_str(&format!("# {}\n\n", prefix.join(" ")));
    }
    walk(target, &prefix, &mut md, &mut commands);

    if path.is_empty() {
        md.push_str("## Error codes\n\n| code | meaning |\n|---|---|\n");
        for (code, meaning) in CODES {
            md.push_str(&format!("| `{code}` | {meaning} |\n"));
        }
    }

    Ok(DocsReport {
        markdown: md,
        commands,
        error_codes: CODES
            .iter()
            .map(|(code, meaning)| ErrorCode { code, meaning })
            .collect(),
    })
}

fn walk(cmd: &mut clap::Command, prefix: &[String], md: &mut String, commands: &mut Vec<String>) {
    let full = prefix.join(" ");
    let has_children = cmd.get_subcommands().any(|c| c.get_name() != "help");
    md.push_str(&format!("### `{full}`\n\n```text\n"));
    md.push_str(cmd.render_long_help().to_string().trim_end());
    md.push_str("\n```\n\n");
    if !has_children {
        commands.push(full.clone());
    }
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .filter(|n| n != "help")
        .collect();
    for name in names {
        let mut next = prefix.to_vec();
        next.push(name.clone());
        let sub = cmd.find_subcommand_mut(&name).expect("listed subcommand");
        walk(sub, &next, md, commands);
    }
}
