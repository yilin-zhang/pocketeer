mod cli;
mod commands;
mod context;
mod human;
mod output;
mod tui;

use std::{io::IsTerminal, process::ExitCode};

use clap::{CommandFactory, Parser};

use crate::{
    cli::{Cli, Command, ConfigCommand, InventoryCommand},
    context::Context,
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

    use commands::{card, cores, library, meta, saves};
    match command {
        Command::Status => out.emit(card::status(&ctx), card::status_text),
        Command::Inventory(InventoryCommand::Refresh) => {
            out.emit(card::inventory_refresh(&ctx), card::refresh_text)
        }
        Command::Bios(cmd) => card::bios(&ctx, &out, cmd, &mut progress),
        Command::Clean { system, apply } => {
            out.emit_outcome(card::clean(&ctx, system, apply), card::clean_report)
        }
        Command::Cores(cmd) => cores::run(&ctx, &out, cmd, &mut progress),
        Command::Library(cmd) => library::run(&ctx, &out, cmd, &mut progress),
        Command::Covers(cmd) => library::covers(&ctx, &out, cmd, &mut progress),
        Command::Card(cmd) => card::roms(&ctx, &out, cmd, &mut progress),
        Command::Saves(cmd) => saves::run(&ctx, &out, cmd, &mut progress),
        Command::Config(cmd) => meta::config(&ctx, &out, cmd),
        Command::Docs { path } => out.emit(meta::docs(&path), meta::docs_text),
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
