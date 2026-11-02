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
        return out.emit::<()>(Err(e), |_| String::new());
    }

    match command {
        Command::Status => out.emit(card::status(&ctx), text::card::status_text),
        Command::Inventory(InventoryCommand::Refresh) => {
            out.emit(card::inventory_refresh(&ctx), text::card::refresh_text)
        }
        Command::Bios(cmd) => run_bios(&ctx, &out, cmd, &mut progress),
        Command::Clean { system, apply } => out.emit_outcome(
            card::clean(&ctx, system, apply.into(), &mut progress),
            text::card::clean_report,
        ),
        Command::Cores(cmd) => run_cores(&ctx, &out, cmd, &mut progress),
        Command::Library(cmd) => run_library(&ctx, &out, cmd, &mut progress),
        Command::Covers(cmd) => run_covers(&ctx, &out, cmd, &mut progress),
        Command::Card(cmd) => run_card(&ctx, &out, cmd, &mut progress),
        Command::Saves(cmd) => run_saves(&ctx, &out, cmd, &mut progress),
        Command::Memories(cmd) => run_memories(&ctx, &out, cmd, &mut progress),
        Command::Config(cmd) => run_config(&ctx, &out, cmd),
        Command::Docs { path } => out.emit(cli::docs(&path), text::docs_text),
    }
}

fn run_bios(
    ctx: &Context,
    out: &Output,
    cmd: BiosCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        BiosCommand::Check { ids, present } => out.emit(
            card::bios_check(ctx, &ids, present),
            text::card::bios_report,
        ),
        BiosCommand::Place { apply } => out.emit_outcome(
            card::bios_place(ctx, apply.into(), progress),
            text::card::place_report,
        ),
    }
}

fn run_cores(
    ctx: &Context,
    out: &Output,
    cmd: CoresCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        CoresCommand::List { outdated } => {
            out.emit(cores::list(ctx, outdated), text::cores::core_list)
        }
        CoresCommand::Search {
            query,
            platform,
            installed,
        } => out.emit(
            cores::search(ctx, query.as_deref(), platform.as_deref(), installed),
            text::cores::search_text,
        ),
        CoresCommand::Info { id } => out.emit(cores::info(ctx, &id), text::cores::core_info),
        CoresCommand::Install(args) => {
            let mode = args.apply.into();
            out.emit_outcome(
                cores::install(ctx, &args.into(), mode, progress),
                text::cores::install_report,
            )
        }
        CoresCommand::Update {
            ids,
            all,
            keep_platforms,
            apply,
        } => out.emit_outcome(
            cores::update(ctx, &ids, all, keep_platforms, apply.into(), progress),
            text::cores::install_report,
        ),
        CoresCommand::Remove { ids, purge, apply } => out.emit_outcome(
            cores::remove(ctx, &ids, purge, apply.into(), progress),
            text::cores::remove_report,
        ),
    }
}

fn run_library(
    ctx: &Context,
    out: &Output,
    cmd: LibraryCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        LibraryCommand::Identify { platform } => out.emit_outcome(
            library::identify(ctx, platform.as_deref(), progress),
            text::library::identify_text,
        ),
        LibraryCommand::Init => out.emit(library::init(ctx), text::library::library_init),
        LibraryCommand::Status => out.emit(library::status(ctx), text::library::library_status),
        LibraryCommand::Import(args) => {
            let mode = args.apply.into();
            let Some(platform) = args.platform else {
                return out.emit_outcome(
                    library::import_sorted(
                        ctx,
                        &args.dir,
                        args.flags.standard_names,
                        !args.flags.no_covers,
                        mode,
                        progress,
                    ),
                    text::library::library_import_sorted,
                );
            };
            let req = library::ImportRequest {
                source: args.dir,
                platform,
                settings: args.flags.into(),
            };
            out.emit_outcome(
                library::import(ctx, &req, mode, progress),
                text::library::library_import,
            )
        }
        LibraryCommand::List { platform, query } => out.emit(
            library::list(ctx, platform.as_deref(), query.as_deref()),
            text::library::library_list,
        ),
        LibraryCommand::Rename {
            rom,
            new_name,
            platform,
            apply,
        } => out.emit_outcome(
            library::rename(ctx, platform.as_deref(), &rom, &new_name, apply.into()),
            text::library::library_rename,
        ),
        LibraryCommand::Remove {
            roms,
            platform,
            apply,
        } => out.emit_outcome(
            library::remove(ctx, platform.as_deref(), &roms, apply.into()),
            text::library::library_remove,
        ),
        LibraryCommand::Standardize { platform, apply } => out.emit_outcome(
            library::standardize(ctx, platform.as_deref(), apply.into()),
            text::library::library_standardize,
        ),
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
        } => out.emit_outcome(
            library::import_cover(ctx, platform.as_deref(), &rom, &image, apply.into()),
            text::library::cover_import,
        ),
        CoversCommand::Fetch {
            platform,
            retry,
            libretro_system,
        } => out.emit(
            library::covers_fetch(
                ctx,
                platform.as_deref(),
                retry,
                libretro_system.as_deref(),
                progress,
            ),
            text::library::covers_fetch_text,
        ),
        CoversCommand::Status => out.emit(
            library::covers_status(ctx),
            text::library::covers_status_text,
        ),
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
            out.emit_outcome(result, text::card::rename_report)
        }
        CardCommand::Copy {
            roms,
            platform,
            apply,
        } => {
            let result = card::plan_copy(ctx, platform.as_deref(), &roms, progress)
                .and_then(|plan| ops::stage(ctx, apply.into(), plan, progress));
            out.emit_outcome(result, text::card::copy_report)
        }
        CardCommand::Import(args) => {
            let mode = args.apply.into();
            out.emit_outcome(
                card::import(ctx, &args.into(), mode, progress),
                text::card::import_report,
            )
        }
        CardCommand::List => {
            out.emit_outcome(card::roms(ctx, None, progress), text::card::roms_text)
        }
        CardCommand::Check { full } => {
            out.emit_outcome(card::roms(ctx, Some(full), progress), text::card::roms_text)
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
        } => out.emit_outcome(
            saves::backup(
                ctx,
                if no_states { Scope::Saves } else { Scope::All },
                conflicts.map(Into::into),
                apply.into(),
                progress,
            ),
            text::saves::saves_sync,
        ),
        SavesCommand::Restore {
            path,
            version,
            apply,
        } => out.emit_outcome(
            saves::restore(ctx, &path, version, apply.into(), progress),
            text::saves::saves_sync,
        ),
        SavesCommand::List => out.emit(saves::list(ctx), text::saves::saves_list),
        SavesCommand::Versions { path } => {
            out.emit(saves::versions(ctx, &path), text::saves::saves_versions)
        }
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
            out.emit_outcome(
                saves::trash(ctx, &path, target, apply.into()),
                text::saves::saves_trash,
            )
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
        MemoriesCommand::List => out.emit(memories::list(ctx), text::memories::memories_list),
        MemoriesCommand::Backup { conflicts, apply } => out.emit_outcome(
            memories::backup(ctx, conflicts.map(Into::into), apply.into(), progress),
            text::saves::saves_sync,
        ),
        MemoriesCommand::Restore { path, apply } => out.emit_outcome(
            memories::restore(ctx, &path, apply.into(), progress),
            text::saves::saves_sync,
        ),
        MemoriesCommand::Trash { path, card, apply } => out.emit_outcome(
            memories::trash(ctx, &path, card, apply.into()),
            text::saves::saves_trash,
        ),
    }
}

fn run_config(ctx: &Context, out: &Output, cmd: ConfigCommand) -> ExitCode {
    use ops::context::{config_get, config_path, config_set};
    match cmd {
        ConfigCommand::Path => out.emit(config_path(ctx), text::config_path_text),
        ConfigCommand::Get { key } => out.emit(config_get(ctx, key.as_deref()), text::config_show),
        ConfigCommand::Set { key, value } => {
            out.emit(config_set(ctx, &key, Some(&value)), text::config_show)
        }
        ConfigCommand::Unset { key } => out.emit(config_set(ctx, &key, None), text::config_show),
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
