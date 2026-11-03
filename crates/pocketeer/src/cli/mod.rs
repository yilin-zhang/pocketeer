//! The command line: its definitions (clap), one module per group of
//! commands, and how a command's outcome is printed ([`output`]). Each group
//! turns its arguments into `ops` calls in one place, runs plans through
//! [`output::stage`], and adds the hints that name its own flags.

mod card;
mod config;
mod cores;
mod library;
pub mod output;
mod saves;

use std::{io::IsTerminal, path::PathBuf, process::ExitCode};

use clap::{Args, CommandFactory, Parser, Subcommand};
use pocketeer_core::{Result, plan::Progress};

use crate::ops::context::Context;
use output::{Mode, Outcome, Output};

pub use card::{BiosCommand, CardCommand};
pub use config::ConfigCommand;
pub use cores::{CoresCommand, InventoryCommand};
pub use library::{CoversCommand, LibraryCommand};
pub use saves::{MemoriesCommand, SavesCommand};

/// Manage cores, ROMs and saves for an Analogue Pocket: a ROM library on
/// this computer, and the SD card you copy ROMs to and back up saves from.
///
/// Run without a subcommand to open the TUI.
/// With --json, every command prints exactly one JSON object to stdout:
/// {"schema_version":2,"ok":true,"data":...} or {"schema_version":2,"ok":false,"error":{...}}.
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

impl From<ApplyArgs> for Mode {
    fn from(args: ApplyArgs) -> Self {
        if args.dry_run {
            Mode::DryRun
        } else {
            Mode::Applied
        }
    }
}

/// Runs the command line: a command, the TUI when there is none, or the
/// usage error.
pub fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => return usage_error(e),
    };
    let Some(command) = cli.command else {
        if cli.global.json || !std::io::stdout().is_terminal() {
            eprintln!(
                "No subcommand given and not running in a terminal, so the TUI can't start.\n"
            );
            let _ = Cli::command().write_help(&mut std::io::stderr());
            return Output::new(cli.global.json).usage(
                "no subcommand given",
                Some("`pocketeer docs` lists every command; the TUI needs a terminal".into()),
            );
        }
        return crate::tui::run(&cli.global);
    };
    let out = Output::new(cli.global.json);
    let ctx = Context::new(cli.global.clone());
    let mut progress = out.progress();
    // A malformed config file is never silently ignored, but the commands
    // that help you find or read the docs still work.
    let needs_config = !matches!(
        command,
        Command::Docs { .. } | Command::Config(ConfigCommand::Path)
    );
    if needs_config && let Err(e) = ctx.config() {
        return out.emit(Err(e));
    }
    out.emit(run(command, &ctx, &mut progress))
}

fn run(command: Command, ctx: &Context, progress: Progress<'_>) -> Result<Outcome> {
    match command {
        Command::Status => card::status(ctx),
        Command::Clean { system, apply } => card::clean(ctx, system, apply.into(), progress),
        Command::Bios(cmd) => card::bios(ctx, cmd, progress),
        Command::Card(cmd) => card::run(ctx, cmd, progress),
        Command::Cores(cmd) => cores::run(ctx, cmd, progress),
        Command::Inventory(InventoryCommand::Refresh) => cores::inventory_refresh(ctx),
        Command::Library(cmd) => library::run(ctx, cmd, progress),
        Command::Covers(cmd) => library::covers(ctx, cmd, progress),
        Command::Saves(cmd) => saves::run(ctx, cmd, progress),
        Command::Memories(cmd) => saves::memories(ctx, cmd, progress),
        Command::Config(cmd) => config::run(ctx, cmd),
        Command::Docs { path } => config::docs(&path),
    }
}

/// clap could not produce a command. Without `--json` this is clap's own
/// behaviour (help/version on stdout and exit 0, errors on stderr and exit 2).
/// With `--json` stdout is reserved for the envelope: everything clap wrote
/// goes to stderr and the envelope carries a `USAGE` error (exit 2).
fn usage_error(e: clap::Error) -> ExitCode {
    use clap::error::ErrorKind;

    let json = std::env::args().skip(1).any(|a| a == "--json");
    if !json {
        e.exit();
    }
    let rendered = e.render().to_string();
    eprint!("{rendered}");
    let out = Output::new(true);
    if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
        return out.usage(
            "help printed to stderr",
            Some("`pocketeer docs --json` returns the reference as data".into()),
        );
    }
    // clap's text: "error: <what>\n\nUsage: …\n\nFor more information, try '--help'."
    let message = rendered
        .split("\n\nUsage:")
        .next()
        .unwrap_or(&rendered)
        .trim()
        .trim_start_matches("error: ")
        .to_string();
    out.usage(
        message,
        Some("`pocketeer docs <command>` shows the full help".into()),
    )
}
