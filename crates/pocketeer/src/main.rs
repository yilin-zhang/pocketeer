//! Entry point: parse the command line, run the matching `ops` function,
//! print its report through `text` (or as JSON). No subcommand opens the TUI.

mod cli;
mod ops;
mod output;
mod text;
mod tui;

use std::{io::IsTerminal, process::ExitCode};

use clap::{CommandFactory, Parser};
use pocketeer_core::{
    card::roms::Depth,
    plan::Event,
    transfer::saves::{Scope, TrashTarget},
};

use crate::{
    cli::{
        BiosCommand, CardCommand, Cli, Command, ConfigCommand, CoresCommand, CoversCommand,
        InventoryCommand, LibraryCommand, MemoriesCommand, SavesCommand,
    },
    ops::{card, context::Context, cores, library, memories, saves},
    output::Output,
};

fn main() -> ExitCode {
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
        return tui::run(&cli.global);
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
        return out.fail(e);
    }

    match command {
        Command::Status => out.emit(card::status(&ctx)),
        Command::Inventory(InventoryCommand::Refresh) => out.emit(card::inventory_refresh(&ctx)),
        Command::Bios(cmd) => run_bios(&ctx, &out, cmd, &mut progress),
        Command::Clean { system, apply } => {
            out.emit(card::clean(&ctx, system, apply.into(), &mut progress))
        }
        Command::Cores(cmd) => run_cores(&ctx, &out, cmd, &mut progress),
        Command::Library(cmd) => run_library(&ctx, &out, cmd, &mut progress),
        Command::Covers(cmd) => run_covers(&ctx, &out, cmd, &mut progress),
        Command::Card(cmd) => run_card(&ctx, &out, cmd, &mut progress),
        Command::Saves(cmd) => run_saves(&ctx, &out, cmd, &mut progress),
        Command::Memories(cmd) => run_memories(&ctx, &out, cmd, &mut progress),
        Command::Config(cmd) => run_config(&ctx, &out, cmd),
        Command::Docs { path } => out.emit(cli::docs(&path)),
    }
}

fn run_bios(
    ctx: &Context,
    out: &Output,
    cmd: BiosCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        BiosCommand::Check { ids, present } => out.emit(card::bios_check(ctx, &ids, present)),
        BiosCommand::Place { apply } => out.emit(card::bios_place(ctx, apply.into(), progress)),
    }
}

fn run_cores(
    ctx: &Context,
    out: &Output,
    cmd: CoresCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        CoresCommand::List { outdated } => out.emit(cores::list(ctx, outdated)),
        CoresCommand::Search {
            query,
            platform,
            installed,
        } => out.emit(cores::search(
            ctx,
            query.as_deref(),
            platform.as_deref(),
            installed,
        )),
        CoresCommand::Info { id } => out.emit(cores::info(ctx, &id)),
        CoresCommand::Install(args) => {
            let mode = args.apply.into();
            out.emit(cores::install(ctx, &args.into(), mode, progress))
        }
        CoresCommand::Update {
            ids,
            all,
            keep_platforms,
            apply,
        } => out.emit(cores::update(
            ctx,
            &ids,
            all,
            keep_platforms,
            apply.into(),
            progress,
        )),
        CoresCommand::Remove { ids, purge, apply } => {
            out.emit(cores::remove(ctx, &ids, purge, apply.into(), progress))
        }
    }
}

fn run_library(
    ctx: &Context,
    out: &Output,
    cmd: LibraryCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        LibraryCommand::Identify { platform } => out.emit(library::identify(
            ctx,
            platform.as_ref().map(std::slice::from_ref),
            progress,
        )),
        LibraryCommand::Init => out.emit(library::init(ctx)),
        LibraryCommand::Status => out.emit(library::status(ctx)),
        LibraryCommand::Import(args) => {
            let req = library::ImportRequest {
                source: args.dir,
                platform: args.platform,
                settings: args.flags.into(),
            };
            out.emit(library::import(ctx, &req, args.apply.into(), progress))
        }
        LibraryCommand::List { platform, query } => {
            out.emit(library::list(ctx, platform.as_deref(), query.as_deref()))
        }
        LibraryCommand::Rename {
            rom,
            new_name,
            platform,
            apply,
        } => out.emit(library::rename(
            ctx,
            platform.as_deref(),
            &rom,
            &new_name,
            apply.into(),
        )),
        LibraryCommand::Remove {
            roms,
            platform,
            apply,
        } => out.emit(library::remove(
            ctx,
            platform.as_deref(),
            &roms,
            apply.into(),
        )),
        LibraryCommand::Standardize { platform, apply } => {
            out.emit(library::standardize(ctx, platform.as_deref(), apply.into()))
        }
    }
}

fn run_covers(
    ctx: &Context,
    out: &Output,
    cmd: CoversCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        CoversCommand::Import {
            rom,
            image,
            platform,
            apply,
        } => out.emit(library::import_cover(
            ctx,
            platform.as_deref(),
            &rom,
            &image,
            apply.into(),
        )),
        CoversCommand::Fetch {
            platform,
            retry,
            libretro_system,
        } => out.emit(library::covers_fetch(
            ctx,
            platform.as_ref().map(std::slice::from_ref),
            retry,
            libretro_system.as_deref(),
            progress,
        )),
        CoversCommand::Status => out.emit(library::covers_status(ctx)),
    }
}

fn run_card(
    ctx: &Context,
    out: &Output,
    cmd: CardCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        CardCommand::Rename {
            path,
            new_name,
            apply,
        } => {
            let result = card::plan_rename(ctx, &path, &new_name)
                .and_then(|plan| ops::stage(ctx, apply.into(), plan, progress));
            out.emit(result)
        }
        CardCommand::Copy {
            roms,
            platform,
            apply,
        } => {
            let result = card::plan_copy(ctx, platform.as_deref(), &roms, progress)
                .and_then(|plan| ops::stage(ctx, apply.into(), plan, progress));
            out.emit(result)
        }
        CardCommand::Import(args) => {
            let mode = args.apply.into();
            out.emit(card::import(ctx, &args.into(), mode, progress))
        }
        CardCommand::List => out.emit(card::roms(ctx, Depth::Cached, progress)),
        CardCommand::Check { full } => {
            let depth = if full { Depth::Full } else { Depth::Sample };
            out.emit(card::roms(ctx, depth, progress))
        }
    }
}

fn run_saves(
    ctx: &Context,
    out: &Output,
    cmd: SavesCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        SavesCommand::Backup {
            no_states,
            conflicts,
            apply,
        } => out.emit(saves::backup(
            ctx,
            if no_states { Scope::Saves } else { Scope::All },
            conflicts.map(Into::into),
            apply.into(),
            progress,
        )),
        SavesCommand::Restore {
            path,
            version,
            apply,
        } => out.emit(saves::restore(ctx, &path, version, apply.into(), progress)),
        SavesCommand::List => out.emit(saves::list(ctx)),
        SavesCommand::Versions { path } => out.emit(saves::versions(ctx, &path)),
        SavesCommand::Trash {
            path,
            card,
            version,
            backup,
            apply,
        } => {
            let target = match (card, version, backup) {
                (true, ..) => TrashTarget::Card,
                (_, Some(id), _) => TrashTarget::Version(id),
                (_, None, true) => TrashTarget::Backup,
                _ => TrashTarget::Library,
            };
            out.emit(saves::trash(ctx, &path, target, apply.into()))
        }
    }
}

fn run_memories(
    ctx: &Context,
    out: &Output,
    cmd: MemoriesCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        MemoriesCommand::List => out.emit(memories::list(ctx)),
        MemoriesCommand::Backup { conflicts, apply } => out.emit(memories::backup(
            ctx,
            conflicts.map(Into::into),
            apply.into(),
            progress,
        )),
        MemoriesCommand::Restore { path, apply } => {
            out.emit(memories::restore(ctx, &path, apply.into(), progress))
        }
        MemoriesCommand::Trash { path, card, apply } => {
            out.emit(memories::trash(ctx, &path, card, apply.into()))
        }
    }
}

fn run_config(ctx: &Context, out: &Output, cmd: ConfigCommand) -> ExitCode {
    use ops::context::{config_get, config_path, config_set};
    match cmd {
        ConfigCommand::Path => out.emit(config_path(ctx)),
        ConfigCommand::Get { key } => out.emit(config_get(ctx, key)),
        ConfigCommand::Set { key, value } => out.emit(config_set(ctx, key, Some(&value))),
        ConfigCommand::Unset { key } => out.emit(config_set(ctx, key, None)),
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
