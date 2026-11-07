//! The command line. Each command is a call or two into `Pocketeer`.

mod tui;

use std::{
    fmt,
    io::{self, IsTerminal},
    ops::ControlFlow,
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::{Parser, Subcommand};
use pocketeer_core::{
    Pocketeer,
    cores::{BiosStatus, CoreRow, SUPPORTED},
    device::{Device, DeviceEntry},
    library::{LibrarySave, LocalEntry, OpenLibraryError},
    log::{FrontEnd, LogEntry},
    platform::Platform,
    pocketeer::{BatchResult, GameRow},
};
use serde::Serialize;
use serde_json::{Value, json};
use strum::IntoEnumIterator;

/// Keeps a Pocket's games and saves in a library on the computer.
///
/// Write a game as `pocketeer games` shows it, which is `<platform>/<name>` in
/// the library and `<platform>/<path>` on the device. A command names a game by
/// where its data comes from.
///
/// A device can hold several copies of a game, but only the first by path is
/// used. Commands that read the device warn about the others on stderr and
/// refuse to act on them.
///
/// The exit code is 0 when everything went well, 1 when part of the command
/// failed, and 2 when it couldn't start.
///
/// With `--json` it prints one object, `{"schema_version": 1, "ok": …, "data":
/// …}`, with `"error": "…"` in place of `data` when it couldn't start. `games`
/// also has `"duplicates": [{"device", "same_game_as"}]`. A malformed command
/// prints text to stderr and nothing to stdout, and exits with 2.
///
/// With no command it opens the terminal UI.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The library, a folder that must exist [default: ~/Pocketeer Library,
    /// created if missing]
    #[arg(long, global = true)]
    library: Option<PathBuf>,
    /// The device [default: the device mounted, if there is one]
    #[arg(long, global = true)]
    device: Option<PathBuf>,
    /// Print JSON, for programs
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List every game, in the library and on the device
    Games,
    /// List the versions of a game's save in the library
    Saves {
        /// A game in the library, as <platform>/<name>
        game: String,
    },
    /// List the saves with no game
    OrphanSaves {
        #[command(subcommand)]
        place: OrphanSavesCommand,
    },
    /// Copy from the device into the library
    Pull {
        #[command(subcommand)]
        what: PullCommand,
    },
    /// Copy from the library onto the device
    Push {
        #[command(subcommand)]
        what: PushCommand,
    },
    /// List the devices mounted
    Device,
    /// Show the device's firmware, free space, cores and games
    Status,
    /// Eject the device
    Eject,
    /// Update the DATs, which name games
    Dat {
        #[command(subcommand)]
        what: DatCommand,
    },
    /// Give a game another name, keeping its extension. Its saves and cover
    /// move with it, and the other side keeps its own name
    Rename {
        #[command(subcommand)]
        what: RenameCommand,
    },
    /// Bring things from the computer into the library
    Import {
        #[command(subcommand)]
        what: ImportCommand,
    },
    /// Put things in the library in the system's trash
    Trash {
        #[command(subcommand)]
        what: TrashCommand,
    },
    /// Delete from the device for good. The library's copies stay
    DeleteOnDevice {
        #[command(subcommand)]
        what: DeleteOnDeviceCommand,
    },
    /// Covers of games in the library
    Cover {
        #[command(subcommand)]
        what: CoverCommand,
    },
    /// The cores on the device, and those that can be installed
    Cores {
        #[command(subcommand)]
        what: CoresCommand,
    },
    /// The files the cores on the device need, which you provide
    Bios {
        #[command(subcommand)]
        what: BiosCommand,
    },
}

#[derive(Subcommand)]
enum RenameCommand {
    /// A game in the library
    Library {
        /// The game, as <platform>/<name>
        game: String,
        /// The new name
        name: String,
    },
    /// A game on the device, which stays in its folder. The Pocket's list of
    /// recent games and any game put to sleep keep the old name
    Device {
        /// The game, as <platform>/<path>
        game: String,
        /// The new name
        name: String,
    },
    /// Games in the library, to their names in the DATs. Saves and covers move
    /// with them and the device isn't touched. Names you chose are replaced
    /// too, and games the DATs don't know stay as they are
    Standard {
        /// The games, as <platform>/<name> [default: every game in the library]
        games: Vec<String>,
    },
}

#[derive(Subcommand)]
enum BiosCommand {
    /// List the files the cores on the device load, and which it has
    List,
    /// Copy a file of yours to where a core loads it from. The file stays, and
    /// nothing on the device is replaced
    Put {
        /// Where it goes, as `bios list` shows it
        bios: String,
        /// The file to copy
        file: PathBuf,
    },
}

#[derive(Subcommand)]
enum CoresCommand {
    /// List the cores that can be installed and those on the device
    List,
    /// Put the newest release of cores on the device
    Install {
        /// Cores, as `cores list` shows them. Quote any with a space in the
        /// name
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Put the newest release on the device for cores it has an older version
    /// of
    Update {
        /// Cores on the device [default: every one with an update]
        ids: Vec<String>,
    },
    /// Delete cores from the device for good. What else they use stays
    Remove {
        /// Cores on the device
        #[arg(required = true)]
        ids: Vec<String>,
    },
}

#[derive(Subcommand)]
enum PullCommand {
    /// Games on the device
    Game {
        /// Games on the device, as <platform>/<path> [default: every one the
        /// library doesn't have]
        games: Vec<String>,
    },
    /// Saves on the device
    Save {
        /// Games on the device, as <platform>/<path> [default: every one]
        games: Vec<String>,
    },
}

#[derive(Subcommand)]
enum PushCommand {
    /// Games in the library
    Game {
        /// Games in the library, as <platform>/<name>
        #[arg(required = true)]
        games: Vec<String>,
    },
    /// A version of a save in the library, onto its game on the device
    Save {
        /// The game in the library, as <platform>/<name>
        game: String,
        /// The version to push, as `pocketeer saves` shows it
        #[arg(long, value_name = "VERSION")]
        at: String,
    },
}

#[derive(Subcommand)]
enum OrphanSavesCommand {
    /// In the library
    Library,
    /// On the device
    Device,
}

#[derive(Subcommand)]
enum TrashCommand {
    /// Games and their covers. Their saves stay
    Game {
        /// Games in the library, as <platform>/<name>
        #[arg(required = true)]
        games: Vec<String>,
    },
    /// Saves, every version or the one given. Their games stay
    Save {
        /// Games in the library, as <platform>/<name>
        #[arg(required = true)]
        games: Vec<String>,
        /// The version to trash, as `pocketeer saves` shows it
        #[arg(long, value_name = "VERSION")]
        at: Option<String>,
    },
    /// Saves with no game, every version
    OrphanSave {
        /// As `pocketeer orphan-saves library` shows them
        #[arg(required = true)]
        saves: Vec<String>,
    },
}

#[derive(Subcommand)]
enum DeleteOnDeviceCommand {
    /// Games, even those the library doesn't have. Their saves stay
    Game {
        /// Games on the device, as <platform>/<path>
        #[arg(required = true)]
        games: Vec<String>,
    },
    /// Saves, even those the library doesn't have
    Save {
        /// Games on the device, as <platform>/<path>
        #[arg(required = true)]
        games: Vec<String>,
    },
    /// Saves with no game, even those the library doesn't have
    OrphanSave {
        /// As `pocketeer orphan-saves device` shows them
        #[arg(required = true)]
        saves: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ImportCommand {
    /// Games from files, or from folders containing games at any depth. Each
    /// gets its DAT name if known and its file name otherwise. The files stay
    Game {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// The platform they are for [default: what each file's extension says]
        #[arg(long)]
        platform: Option<String>,
    },
    /// A save from a file, as a version of a game's save dated when the file
    /// was written. The file stays
    Save {
        /// The game in the library, as <platform>/<name>
        game: String,
        /// The save
        file: PathBuf,
    },
}

#[derive(Subcommand)]
enum DatCommand {
    /// Download the DATs that changed
    Sync,
}

#[derive(Subcommand)]
enum CoverCommand {
    /// Download a cover for each game that has none
    Fetch {
        /// Games in the library, as <platform>/<name> [default: every one
        /// without a cover]
        games: Vec<String>,
    },
    /// Make a PNG, JPEG or WebP file a game's cover. An existing cover goes to
    /// the system's trash, and the file stays where it is
    Import {
        /// The game in the library, as <platform>/<name>
        game: String,
        /// The image
        image: PathBuf,
    },
}

#[derive(Clone, Copy)]
enum PrintFormat {
    Text,
    Json,
}

/// Why a command didn't all go well.
enum CliError {
    /// It couldn't start for this reason, which hasn't been printed yet.
    CouldNotStart(String),
    /// It ran and part of it failed, which it has already printed.
    SomeFailed,
}

impl<E: std::error::Error> From<E> for CliError {
    fn from(error: E) -> Self {
        CliError::CouldNotStart(error.to_string())
    }
}

type CliResult<T> = Result<T, CliError>;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Some(command) = &cli.command else {
        return run_terminal_ui(&cli);
    };
    let format = if cli.json {
        PrintFormat::Json
    } else {
        PrintFormat::Text
    };
    match run_command(&cli, command, format) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::SomeFailed) => ExitCode::from(1),
        Err(CliError::CouldNotStart(message)) => {
            match format {
                PrintFormat::Json => print_envelope(json!({
                    "ok": false,
                    "error": message,
                })),
                PrintFormat::Text => eprintln!("error: {message}"),
            }
            ExitCode::from(2)
        }
    }
}

fn print_json(ok: bool, data: Value) {
    print_envelope(json!({ "ok": ok, "data": data }));
}

fn print_envelope(mut envelope: Value) {
    envelope["schema_version"] = json!(1);
    println!("{envelope}");
}

/// Opens the terminal UI when no command is given, if this is a terminal.
fn run_terminal_ui(cli: &Cli) -> ExitCode {
    if cli.json || !io::stdout().is_terminal() {
        eprintln!("error: the terminal UI needs a terminal; give a command, or see --help");
        return ExitCode::from(2);
    }
    match library_path(cli) {
        Ok(library) => tui::run(library, cli.device.clone()),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}

/// The library given, or else the default one.
fn library_path(cli: &Cli) -> Result<PathBuf, OpenLibraryError> {
    match &cli.library {
        Some(library) => Ok(library.clone()),
        None => Pocketeer::default_library_path(),
    }
}

fn run_command(cli: &Cli, command: &Command, format: PrintFormat) -> CliResult<()> {
    let open = |device_use| CliSession::open(cli, device_use, format);
    match command {
        Command::Device => print_devices(format),
        Command::Status => open(DeviceUse::ScannedRequired)?.print_status(),
        Command::Games => open(DeviceUse::ScannedIfMounted)?.print_games(),
        Command::Saves { game } => open(DeviceUse::NotNeeded)?.print_saves(game),
        Command::OrphanSaves { place } => match place {
            OrphanSavesCommand::Library => {
                open(DeviceUse::NotNeeded)?.print_orphan_saves_in_library()
            }
            OrphanSavesCommand::Device => {
                open(DeviceUse::UnscannedRequired)?.print_orphan_saves_on_device()
            }
        },
        Command::Pull { what } => match what {
            PullCommand::Game { games } => open(DeviceUse::ScannedRequired)?.pull_games(games),
            PullCommand::Save { games } => open(DeviceUse::ScannedRequired)?.pull_saves(games),
        },
        Command::Push { what } => match what {
            PushCommand::Game { games } => open(DeviceUse::ScannedRequired)?.push_games(games),
            PushCommand::Save { game, at } => open(DeviceUse::ScannedRequired)?.push_save(game, at),
        },
        Command::Import { what } => match what {
            ImportCommand::Game { paths, platform } => {
                open(DeviceUse::NotNeeded)?.import_games(paths, platform.as_deref())
            }
            ImportCommand::Save { game, file } => {
                open(DeviceUse::NotNeeded)?.import_save(game, file)
            }
        },
        Command::Trash { what } => match what {
            TrashCommand::Game { games } => open(DeviceUse::NotNeeded)?.trash_games(games),
            TrashCommand::Save { games, at } => {
                open(DeviceUse::NotNeeded)?.trash_saves(games, at.as_deref())
            }
            TrashCommand::OrphanSave { saves } => {
                open(DeviceUse::NotNeeded)?.trash_orphan_saves(saves)
            }
        },
        Command::DeleteOnDevice { what } => match what {
            DeleteOnDeviceCommand::Game { games } => {
                open(DeviceUse::ScannedRequired)?.delete_games(games)
            }
            DeleteOnDeviceCommand::Save { games } => {
                open(DeviceUse::ScannedRequired)?.delete_saves(games)
            }
            DeleteOnDeviceCommand::OrphanSave { saves } => {
                open(DeviceUse::UnscannedRequired)?.delete_orphan_saves(saves)
            }
        },
        Command::Cores { what } => match what {
            CoresCommand::List => open(DeviceUse::UnscannedIfMounted)?.print_cores(),
            CoresCommand::Install { ids } => open(DeviceUse::UnscannedRequired)?.install_cores(ids),
            CoresCommand::Update { ids } => open(DeviceUse::UnscannedRequired)?.update_cores(ids),
            CoresCommand::Remove { ids } => open(DeviceUse::UnscannedRequired)?.remove_cores(ids),
        },
        Command::Rename { what } => match what {
            RenameCommand::Library { game, name } => {
                open(DeviceUse::NotNeeded)?.rename_game_in_library(game, name)
            }
            RenameCommand::Device { game, name } => {
                open(DeviceUse::ScannedRequired)?.rename_game_on_device(game, name)
            }
            RenameCommand::Standard { games } => {
                open(DeviceUse::NotNeeded)?.rename_games_to_dat_names(games)
            }
        },
        Command::Bios { what } => match what {
            BiosCommand::List => open(DeviceUse::UnscannedRequired)?.print_bios(),
            BiosCommand::Put { bios, file } => {
                open(DeviceUse::UnscannedRequired)?.put_bios(bios, file)
            }
        },
        Command::Eject => open(DeviceUse::UnscannedRequired)?.eject(),
        Command::Dat {
            what: DatCommand::Sync,
        } => open(DeviceUse::NotNeeded)?.sync_dats(),
        Command::Cover { what } => match what {
            CoverCommand::Fetch { games } => open(DeviceUse::NotNeeded)?.fetch_covers(games),
            CoverCommand::Import { game, image } => {
                open(DeviceUse::NotNeeded)?.import_cover(game, image)
            }
        },
    }
}

/// What a command needs from the device. Opening and especially scanning can be
/// slow, so commands that need neither skip them.
enum DeviceUse {
    NotNeeded,
    /// Its games if there is one. No device is fine.
    ScannedIfMounted,
    /// Its games. No device is an error.
    ScannedRequired,
    /// Its folders but not its games. No device is an error.
    UnscannedRequired,
    /// Its folders but not its games, if there is one. No device is fine.
    UnscannedIfMounted,
}

fn print_devices(format: PrintFormat) -> CliResult<()> {
    let found: Vec<String> = Device::find_mounted()
        .iter()
        .map(|device_root| device_root.display().to_string())
        .collect();
    match format {
        PrintFormat::Json => print_json(true, json!({ "found": found })),
        PrintFormat::Text if found.is_empty() => println!("no device mounted"),
        PrintFormat::Text => found
            .iter()
            .for_each(|device_root| println!("{device_root}")),
    }
    Ok(())
}

/// The state a command runs with. Commands do their own printing.
struct CliSession {
    pocketeer: Pocketeer,
    format: PrintFormat,
    /// What the command has done so far, to report at the end.
    entries: Vec<LogEntry>,
    /// The copies the scan of the device left out.
    duplicates: Vec<DeviceEntry>,
}

impl CliSession {
    /// The library, and the device given or the one mounted as `device_use`
    /// says.
    fn open(cli: &Cli, device_use: DeviceUse, format: PrintFormat) -> CliResult<Self> {
        let mut pocketeer = Pocketeer::open(&library_path(cli)?, FrontEnd::Cli)?;
        let mut duplicates = Vec::new();
        if !matches!(device_use, DeviceUse::NotNeeded) {
            let opened = match &cli.device {
                Some(device) => pocketeer.open_device(device).map(|()| true)?,
                None => pocketeer.open_mounted_device()?,
            };
            match (opened, device_use) {
                (false, DeviceUse::ScannedIfMounted | DeviceUse::UnscannedIfMounted)
                | (true, DeviceUse::UnscannedRequired | DeviceUse::UnscannedIfMounted) => {}
                (false, _) => {
                    return Err(CliError::CouldNotStart(
                        "no device found; give one with --device".to_string(),
                    ));
                }
                (true, _) => duplicates = pocketeer.scan_device()?.duplicates,
            }
        }
        let session = Self {
            pocketeer,
            format,
            entries: Vec::new(),
            duplicates,
        };
        for duplicate in &session.duplicates {
            eprintln!(
                "warning: {duplicate} is the same game as {}; it is left out and its save isn't synced",
                session.first_copy(duplicate),
            );
        }
        Ok(session)
    }

    /// The copy of the game the scan kept in place of `duplicate`.
    fn first_copy(&self, duplicate: &DeviceEntry) -> &DeviceEntry {
        let device = self.pocketeer.device().expect("the device is open");
        let first = device.entry_for(&duplicate.game);
        first.expect("the scan keeps a copy of each game")
    }

    fn print_games(&self) -> CliResult<()> {
        #[derive(Serialize)]
        struct Row {
            platform: String,
            library: Option<String>,
            device: Option<String>,
            saves_kept: usize,
            new_save_on_device: bool,
            /// For a game only on the device, what pulling it names it.
            pulls_as: Option<String>,
            /// The cover's file, for a game in the library that has one.
            cover: Option<PathBuf>,
        }
        let rows: Vec<Row> = self
            .pocketeer
            .game_rows()
            .iter()
            .map(|row| {
                let id = row.game().platform.id();
                let pulls_as = match row {
                    GameRow::OnDevice(on_device) => {
                        Some(format!("{id}/{}", self.pocketeer.name_for_pull(on_device)))
                    }
                    _ => None,
                };
                Row {
                    platform: id.to_string(),
                    library: row.local_entry().map(ToString::to_string),
                    device: row.device_entry().map(ToString::to_string),
                    saves_kept: row.saves_kept(),
                    new_save_on_device: row.has_new_save_on_device(),
                    pulls_as,
                    cover: row.local_entry().and_then(|local| local.cover_path.clone()),
                }
            })
            .collect();
        match self.format {
            PrintFormat::Json => {
                let duplicates: Vec<Value> = self
                    .duplicates
                    .iter()
                    .map(|duplicate| {
                        json!({
                            "device": duplicate.to_string(),
                            "same_game_as": self.first_copy(duplicate).to_string(),
                        })
                    })
                    .collect();
                print_envelope(json!({ "ok": true, "data": rows, "duplicates": duplicates }));
            }
            PrintFormat::Text => {
                let cells: Vec<[String; 4]> = rows
                    .iter()
                    .map(|row| {
                        let save = match (row.saves_kept, row.new_save_on_device) {
                            (0, false) => "-".to_string(),
                            (0, true) => "new on the device".to_string(),
                            (saves_kept, false) => format!("{saves_kept} kept"),
                            (saves_kept, true) => format!("{saves_kept} kept, new on the device"),
                        };
                        let or_dash =
                            |text: Option<&String>| text.cloned().unwrap_or_else(|| "-".into());
                        let pulls_as = row.pulls_as.clone().unwrap_or_default();
                        [
                            or_dash(row.library.as_ref()),
                            or_dash(row.device.as_ref()),
                            save,
                            pulls_as,
                        ]
                    })
                    .collect();
                if self.pocketeer.device().is_none() {
                    println!("(no device)");
                }
                print!(
                    "{}",
                    table(&["LIBRARY", "DEVICE", "SAVE", "PULLS AS"], &cells)
                );
            }
        }
        Ok(())
    }

    fn print_saves(&self, game: &str) -> CliResult<()> {
        #[derive(Serialize)]
        struct Version {
            version: String,
            size: u64,
            newest: bool,
        }
        let saves = self.local_entry(game)?.save.as_ref();
        let versions: Vec<Version> = saves
            .into_iter()
            .flat_map(|saves| saves.iter())
            .enumerate()
            .map(|(i, version)| Version {
                version: version.name.clone(),
                size: version.save.size,
                newest: i == 0,
            })
            .collect();
        match self.format {
            PrintFormat::Json => print_json(true, json!(versions)),
            PrintFormat::Text if versions.is_empty() => println!("no save in the library"),
            PrintFormat::Text => {
                let cells: Vec<[String; 3]> = versions
                    .iter()
                    .map(|version| {
                        let note = if version.newest { "newest" } else { "" };
                        [
                            version.version.clone(),
                            format!("{} bytes", version.size),
                            note.into(),
                        ]
                    })
                    .collect();
                print!(
                    "{}",
                    table_right_aligned(&["VERSION", "SIZE", ""], &cells, 1)
                );
            }
        }
        Ok(())
    }

    fn print_orphan_saves_in_library(&self) -> CliResult<()> {
        #[derive(Serialize)]
        struct Orphan {
            save: String,
            versions: usize,
            newest: String,
        }
        let orphans: Vec<Orphan> = self
            .pocketeer
            .library()
            .find_orphan_saves()?
            .into_iter()
            .map(|orphan| Orphan {
                save: orphan.to_string(),
                versions: orphan.save.iter().count(),
                newest: orphan.save.newest().name.clone(),
            })
            .collect();
        match self.format {
            PrintFormat::Json => print_json(true, json!(orphans)),
            PrintFormat::Text if orphans.is_empty() => println!("no orphan saves"),
            PrintFormat::Text => {
                let cells: Vec<[String; 3]> = orphans
                    .iter()
                    .map(|orphan| {
                        [
                            orphan.save.clone(),
                            orphan.versions.to_string(),
                            orphan.newest.clone(),
                        ]
                    })
                    .collect();
                print!("{}", table(&["SAVE", "VERSIONS", "NEWEST"], &cells));
            }
        }
        Ok(())
    }

    fn print_orphan_saves_on_device(&self) -> CliResult<()> {
        #[derive(Serialize)]
        struct Orphan {
            save: String,
            size: u64,
            written: String,
        }
        let orphans: Vec<Orphan> = self
            .pocketeer
            .find_orphan_saves_on_device()?
            .into_iter()
            .map(|orphan| Orphan {
                save: orphan.to_string(),
                size: orphan.save.size,
                written: orphan.save.timestamp(),
            })
            .collect();
        match self.format {
            PrintFormat::Json => print_json(true, json!(orphans)),
            PrintFormat::Text if orphans.is_empty() => println!("no orphan saves"),
            PrintFormat::Text => {
                let cells: Vec<[String; 3]> = orphans
                    .iter()
                    .map(|orphan| {
                        [
                            orphan.save.clone(),
                            format!("{} bytes", orphan.size),
                            orphan.written.clone(),
                        ]
                    })
                    .collect();
                print!(
                    "{}",
                    table_right_aligned(&["SAVE", "SIZE", "WRITTEN"], &cells, 1)
                );
            }
        }
        Ok(())
    }

    fn pull_games(&mut self, game_args: &[String]) -> CliResult<()> {
        let games = match game_args {
            [] => self.pocketeer.games_only_on_device(),
            game_args => self.entries_on_device(game_args)?,
        };
        self.run_batch(|pocketeer, on_logged| pocketeer.pull_games(&games, on_logged))
    }

    fn pull_saves(&mut self, game_args: &[String]) -> CliResult<()> {
        let games = match game_args {
            [] => self.games_with_saves_to_pull(),
            game_args => self.entries_on_device(game_args)?,
        };
        self.run_batch(|pocketeer, on_logged| pocketeer.pull_saves(&games, on_logged))
    }

    /// The games on the device that the library has. Nobody asked for the
    /// others by name, so they aren't failures, but saves only they have are
    /// reported.
    fn games_with_saves_to_pull(&self) -> Vec<DeviceEntry> {
        let others = self.pocketeer.games_only_on_device();
        let unsaved: Vec<String> = others
            .iter()
            .filter(|game| game.save.is_some())
            .map(ToString::to_string)
            .collect();
        if !unsaved.is_empty() {
            eprintln!(
                "warning: these games on the device have saves but aren't in the library, so their saves weren't backed up: {}",
                unsaved.join(", ")
            );
        }
        self.pocketeer.games_on_device_in_library()
    }

    fn push_games(&mut self, game_args: &[String]) -> CliResult<()> {
        let games = self.entries_in_library(game_args)?;
        self.run_batch(|pocketeer, on_logged| pocketeer.push_games(&games, on_logged))
    }

    fn push_save(&mut self, game_arg: &str, version_name: &str) -> CliResult<()> {
        let local = self.local_entry(game_arg)?;
        let version = version_named(game_arg, local, version_name)?;
        let device = self.pocketeer.device().expect("the device is open");
        let copy = device.entry_for(&local.game).cloned();
        let copy = copy.ok_or_else(|| not_on_device(game_arg))?;
        let versions = [(copy, version)];
        self.run_batch(|pocketeer, on_logged| pocketeer.push_save_versions(&versions, on_logged))
    }

    fn import_games(&mut self, paths: &[PathBuf], platform: Option<&str>) -> CliResult<()> {
        let platform = platform.map(parse_platform).transpose()?;
        let files = pocketeer_core::pocketeer::expand_import_paths(paths)?;
        self.run_batch(|pocketeer, on_logged| {
            pocketeer.import_games(&files, platform.as_ref(), on_logged)
        })
    }

    fn import_save(&mut self, game: &str, file: &Path) -> CliResult<()> {
        let saves = [(self.local_entry(game)?.clone(), file.to_path_buf())];
        self.run_batch(|pocketeer, on_logged| pocketeer.import_saves(&saves, on_logged))
    }

    fn import_cover(&mut self, game: &str, image: &Path) -> CliResult<()> {
        let covers = [(self.local_entry(game)?.clone(), image.to_path_buf())];
        self.run_batch(|pocketeer, on_logged| pocketeer.import_covers(&covers, on_logged))
    }

    fn trash_games(&mut self, game_args: &[String]) -> CliResult<()> {
        let games = self.entries_in_library(game_args)?;
        self.run_batch(|pocketeer, on_logged| pocketeer.trash_games_in_library(&games, on_logged))
    }

    fn trash_saves(&mut self, game_args: &[String], at: Option<&str>) -> CliResult<()> {
        let games = self.entries_in_library(game_args)?;
        let Some(version_name) = at else {
            return self.run_batch(|pocketeer, on_logged| {
                pocketeer.trash_saves_in_library(&games, on_logged)
            });
        };
        let versions = game_args
            .iter()
            .zip(games)
            .map(|(game_arg, game)| {
                let version = version_named(game_arg, &game, version_name)?;
                Ok((game, version))
            })
            .collect::<CliResult<Vec<(LocalEntry, LibrarySave)>>>()?;
        self.run_batch(|pocketeer, on_logged| {
            pocketeer.trash_save_versions_in_library(&versions, on_logged)
        })
    }

    fn trash_orphan_saves(&mut self, orphan_args: &[String]) -> CliResult<()> {
        let orphans = self.pocketeer.library().find_orphan_saves()?;
        let orphans = pick_orphans(orphan_args, &orphans)?;
        self.run_batch(|pocketeer, on_logged| {
            pocketeer.trash_orphan_saves_in_library(&orphans, on_logged)
        })
    }

    fn delete_games(&mut self, game_args: &[String]) -> CliResult<()> {
        let games = self.entries_on_device(game_args)?;
        self.run_batch(|pocketeer, on_logged| pocketeer.delete_games_on_device(&games, on_logged))
    }

    fn delete_saves(&mut self, game_args: &[String]) -> CliResult<()> {
        let games = self.entries_on_device(game_args)?;
        self.run_batch(|pocketeer, on_logged| pocketeer.delete_saves_on_device(&games, on_logged))
    }

    fn delete_orphan_saves(&mut self, orphan_args: &[String]) -> CliResult<()> {
        let orphans = self.pocketeer.find_orphan_saves_on_device()?;
        let orphans = pick_orphans(orphan_args, &orphans)?;
        self.run_batch(|pocketeer, on_logged| {
            pocketeer.delete_orphan_saves_on_device(&orphans, on_logged)
        })
    }

    fn fetch_covers(&mut self, game_args: &[String]) -> CliResult<()> {
        let games = match game_args {
            [] => self.pocketeer.games_without_covers(),
            game_args => self.entries_in_library(game_args)?,
        };
        self.run_batch(|pocketeer, on_logged| pocketeer.fetch_covers(&games, on_logged))
    }

    /// Runs a command over games. As text each result prints as it happens,
    /// because a device is slow and this shows progress. As JSON they all print
    /// at the end.
    fn run_batch(
        &mut self,
        run: impl FnOnce(
            &mut Pocketeer,
            &mut dyn FnMut(&Pocketeer, LogEntry) -> ControlFlow<()>,
        ) -> BatchResult,
    ) -> CliResult<()> {
        let (format, entries) = (self.format, &mut self.entries);
        let batch_result = run(&mut self.pocketeer, &mut |_, entry| {
            if matches!(format, PrintFormat::Text) {
                println!("{}", entry.report.to_line());
            }
            entries.push(entry);
            ControlFlow::Continue(())
        });
        self.finish_batch(batch_result)
    }

    /// Finishes a command over games. `batch_result` says whether it stopped
    /// early.
    fn finish_batch(&self, batch_result: BatchResult) -> CliResult<()> {
        if let Err(error) = &batch_result {
            eprintln!("error: {error}");
        }
        let ok = batch_result.is_ok() && self.entries.iter().all(|error| error.report.is_ok);
        if matches!(self.format, PrintFormat::Json) {
            let lines: Vec<Value> = self.entries.iter().map(LogEntry::to_json).collect();
            print_json(ok, json!(lines));
        }
        if ok {
            Ok(())
        } else {
            Err(CliError::SomeFailed)
        }
    }

    fn print_cores(&mut self) -> CliResult<()> {
        #[derive(Serialize)]
        struct Row {
            id: String,
            platforms: Vec<String>,
            installed: bool,
            /// None if it isn't installed, or its `core.json` can't be read.
            version: Option<String>,
            latest: Option<String>,
            update_available: bool,
        }
        let core_list = self.pocketeer.load_core_list()?;
        if let Some(failure) = &core_list.index_failure {
            eprintln!("warning: couldn't get the cores that can be installed: {failure}");
        }
        let rows: Vec<Row> = core_list
            .rows
            .iter()
            .map(|row: &CoreRow| {
                let platforms = match (row.latest(), row.installed()) {
                    (Some(latest), _) => &latest.platforms,
                    (None, Some(installed)) => &installed.platforms,
                    (None, None) => unreachable!("a core is on one side or the other"),
                };
                Row {
                    id: row.id().to_string(),
                    platforms: platforms
                        .iter()
                        .map(|platform| platform.id().to_string())
                        .collect(),
                    installed: row.installed().is_some(),
                    version: row.installed().and_then(|i| i.version.clone()),
                    latest: row.latest().map(|latest| latest.version.clone()),
                    update_available: row.has_update(),
                }
            })
            .collect();
        match self.format {
            PrintFormat::Json => print_json(true, json!(rows)),
            PrintFormat::Text => {
                let cells: Vec<[String; 4]> = rows
                    .iter()
                    .map(|row| {
                        let on_device = match (row.installed, &row.version) {
                            (false, _) => "-".to_string(),
                            (true, Some(version)) => version.clone(),
                            (true, None) => "unknown version".to_string(),
                        };
                        let latest = row.latest.clone().unwrap_or_else(|| "-".to_string());
                        [row.id.clone(), row.platforms.join(", "), on_device, latest]
                    })
                    .collect();
                if self.pocketeer.device().is_none() {
                    println!("(no device)");
                }
                print!(
                    "{}",
                    table(&["CORE", "PLATFORM", "ON THE DEVICE", "LATEST"], &cells)
                );
            }
        }
        Ok(())
    }

    fn install_cores(&mut self, core_ids: &[String]) -> CliResult<()> {
        if let Some(id) = core_ids.iter().find(|id| !SUPPORTED.contains(&id.as_str())) {
            let message = format!("{id}: not a core that can be installed, as `cores list` shows");
            return Err(CliError::CouldNotStart(message));
        }
        self.run_batch(|pocketeer, on_logged| pocketeer.install_cores(core_ids, on_logged))
    }

    fn update_cores(&mut self, core_ids: &[String]) -> CliResult<()> {
        let core_ids = match core_ids {
            [] => self.pocketeer.find_cores_with_updates()?,
            core_ids => {
                self.ensure_cores_on_device(core_ids)?;
                core_ids.to_vec()
            }
        };
        if core_ids.is_empty() && matches!(self.format, PrintFormat::Text) {
            println!("every core is up to date");
        }
        self.run_batch(|pocketeer, on_logged| pocketeer.install_cores(&core_ids, on_logged))
    }

    fn remove_cores(&mut self, core_ids: &[String]) -> CliResult<()> {
        self.ensure_cores_on_device(core_ids)?;
        self.run_batch(|pocketeer, on_logged| pocketeer.remove_cores(core_ids, on_logged))
    }

    /// Refuses cores the device doesn't have.
    fn ensure_cores_on_device(&self, core_ids: &[String]) -> CliResult<()> {
        let device = self.pocketeer.device().expect("the device is open");
        let cores = device.installed_cores()?;
        match core_ids
            .iter()
            .find(|id| !cores.iter().any(|core| core.id == **id))
        {
            Some(id) => Err(CliError::CouldNotStart(format!("{id}: not on the device"))),
            None => Ok(()),
        }
    }

    fn rename_game_in_library(&mut self, game: &str, name: &str) -> CliResult<()> {
        let games = [(self.local_entry(game)?.clone(), name.to_string())];
        self.run_batch(|pocketeer, on_logged| pocketeer.rename_games_in_library(&games, on_logged))
    }

    fn rename_games_to_dat_names(&mut self, game_args: &[String]) -> CliResult<()> {
        let games = match game_args {
            [] => self.pocketeer.library().entries().to_vec(),
            game_args => self.entries_in_library(game_args)?,
        };
        let renames = self.pocketeer.renames_to_dat_names(&games);
        if renames.is_empty() && matches!(self.format, PrintFormat::Text) {
            println!("no game to rename");
        }
        self.run_batch(|pocketeer, on_logged| {
            pocketeer.rename_games_in_library(&renames, on_logged)
        })
    }

    fn rename_game_on_device(&mut self, game: &str, name: &str) -> CliResult<()> {
        let on_device = self.entries_on_device(&[game.to_string()])?.remove(0);
        let games = [(on_device, name.to_string())];
        self.run_batch(|pocketeer, on_logged| pocketeer.rename_games_on_device(&games, on_logged))
    }

    fn print_status(&self) -> CliResult<()> {
        let device = self.pocketeer.device().expect("the device is open");
        let firmware = device.read_firmware();
        let space = device.read_disk_space()?;
        let core_count = device.installed_cores()?.len();
        let game_count = device.entries().len();
        let root = device.root().display().to_string();
        match self.format {
            PrintFormat::Json => print_json(
                true,
                json!({
                    "root": root,
                    "firmware": firmware.as_ref().map(|firmware| json!({
                        "version": firmware.version,
                        "build_date": firmware.build_date,
                    })),
                    "total_bytes": space.total_bytes,
                    "available_bytes": space.available_bytes,
                    "cores": core_count,
                    "games": game_count,
                }),
            ),
            PrintFormat::Text => {
                let firmware =
                    firmware.map_or("unknown".to_string(), |firmware| firmware.to_string());
                println!("root      {root}");
                println!("firmware  {firmware}");
                println!("space     {space}");
                println!("cores     {core_count}");
                println!("games     {game_count}");
            }
        }
        Ok(())
    }

    fn print_bios(&self) -> CliResult<()> {
        #[derive(Serialize)]
        struct NeededBy {
            core: String,
            required: bool,
        }
        #[derive(Serialize)]
        struct Row {
            file: String,
            status: &'static str,
            /// Its size on the device, if that is what is wrong with it.
            size: Option<u64>,
            size_exact: Option<u64>,
            required: bool,
            needed_by: Vec<NeededBy>,
        }
        let rows: Vec<Row> = self
            .pocketeer
            .read_bios_rows()?
            .iter()
            .map(|row| {
                let (status, size) = match row.status {
                    BiosStatus::Present => ("present", None),
                    BiosStatus::WrongSize(size) => ("wrong_size", Some(size)),
                    BiosStatus::Missing => ("missing", None),
                };
                let needed_by = row.needed_by.iter().map(|(core, required)| NeededBy {
                    core: core.clone(),
                    required: *required,
                });
                Row {
                    file: row.to_string(),
                    status,
                    size,
                    size_exact: row.size_exact,
                    required: row.is_required(),
                    needed_by: needed_by.collect(),
                }
            })
            .collect();
        match self.format {
            PrintFormat::Json => print_json(true, json!(rows)),
            PrintFormat::Text if rows.is_empty() => {
                println!("no core on the device needs a file");
            }
            PrintFormat::Text => {
                let cells: Vec<[String; 3]> = rows
                    .iter()
                    .map(|row| {
                        let status = match (row.status, row.size, row.size_exact) {
                            ("wrong_size", Some(size), Some(exact)) => {
                                format!("wrong size: {size} bytes, not {exact}")
                            }
                            ("missing", ..) if !row.required => "missing, optional".to_string(),
                            (status, ..) => status.to_string(),
                        };
                        let cores: Vec<&str> = row
                            .needed_by
                            .iter()
                            .map(|needed_by| needed_by.core.as_str())
                            .collect();
                        [row.file.clone(), status, cores.join(", ")]
                    })
                    .collect();
                print!("{}", table(&["FILE", "STATUS", "NEEDED BY"], &cells));
            }
        }
        Ok(())
    }

    fn put_bios(&mut self, file: &str, from: &Path) -> CliResult<()> {
        let rows = self.pocketeer.read_bios_rows()?;
        let Some(row) = rows.into_iter().find(|row| row.to_string() == file) else {
            let message = format!("{file}: no core on the device loads it, as `bios list` shows");
            return Err(CliError::CouldNotStart(message));
        };
        let files = [(row, from.to_path_buf())];
        self.run_batch(|pocketeer, on_logged| pocketeer.put_bios_files(&files, on_logged))
    }

    fn eject(&mut self) -> CliResult<()> {
        let root = self
            .pocketeer
            .device()
            .map(|device| device.root().to_path_buf());
        self.pocketeer.eject_device()?;
        match self.format {
            PrintFormat::Json => {
                let root = root.map(|root| root.display().to_string());
                print_json(true, json!({ "ejected": root }));
            }
            PrintFormat::Text => println!("ejected"),
        }
        Ok(())
    }

    fn sync_dats(&mut self) -> CliResult<()> {
        let report = self.pocketeer.sync_dats()?;
        let platform_ids = |platforms: &[Platform]| {
            platforms
                .iter()
                .map(|platform| platform.id().to_string())
                .collect::<Vec<_>>()
        };
        let (updated, missing) = (platform_ids(&report.updated), platform_ids(&report.missing));
        match self.format {
            PrintFormat::Json => {
                print_json(true, json!({ "updated": updated, "missing": missing }));
            }
            PrintFormat::Text => {
                if updated.is_empty() {
                    println!("the DATs are up to date");
                } else {
                    println!("updated: {}", updated.join(", "));
                }
                if !missing.is_empty() {
                    println!("no longer available: {}", missing.join(", "));
                }
            }
        }
        Ok(())
    }

    /// The device's games named in `game_args`. A copy the scan left out is
    /// refused, so that nothing meant for one file happens to another.
    fn entries_on_device(&self, game_args: &[String]) -> CliResult<Vec<DeviceEntry>> {
        let device = self.pocketeer.device().expect("the device is open");
        game_args
            .iter()
            .map(|game_arg| {
                let (platform, path) = split_platform(game_arg)?;
                let path = Path::new(path);
                let is_named_copy = |on_device: &&DeviceEntry| {
                    on_device.game.platform == platform && on_device.path == path
                };
                if let Some(duplicate) = self.duplicates.iter().find(is_named_copy) {
                    let first = self.first_copy(duplicate).to_string();
                    let message =
                        format!("{game_arg}: the same game as {first}, the copy that is used");
                    return Err(CliError::CouldNotStart(message));
                }
                let found = device.entry_at(&platform, path);
                found.cloned().ok_or_else(|| not_on_device(game_arg))
            })
            .collect()
    }

    fn entries_in_library(&self, game_args: &[String]) -> CliResult<Vec<LocalEntry>> {
        game_args
            .iter()
            .map(|game_arg| self.local_entry(game_arg).cloned())
            .collect()
    }

    fn local_entry(&self, game_arg: &str) -> CliResult<&LocalEntry> {
        let (platform, name) = split_platform(game_arg)?;
        let not_there = || CliError::CouldNotStart(format!("{game_arg}: not in the library"));
        self.pocketeer
            .library()
            .entry_named(&platform, name)
            .ok_or_else(not_there)
    }
}

fn not_on_device(game_arg: &str) -> CliError {
    CliError::CouldNotStart(format!("{game_arg}: not on the device"))
}

/// The version of `local`'s save called `version_name`, as `saves` shows it.
fn version_named(game_arg: &str, local: &LocalEntry, version_name: &str) -> CliResult<LibrarySave> {
    let saves = local.save.as_ref();
    let version = saves.and_then(|saves| saves.version_named(version_name));
    version.cloned().ok_or_else(|| {
        let message = format!("{game_arg}: no such version, as `pocketeer saves` shows them");
        CliError::CouldNotStart(message)
    })
}

/// The orphan saves named in `orphan_args`, as `orphan-saves` shows them.
/// Nothing else is accepted, so a game's save can't be taken for an orphan.
fn pick_orphans<T: Clone + fmt::Display>(
    orphan_args: &[String],
    orphans: &[T],
) -> CliResult<Vec<T>> {
    orphan_args
        .iter()
        .map(|orphan_arg| {
            let found = orphans
                .iter()
                .find(|orphan| orphan.to_string() == *orphan_arg)
                .cloned();
            found.ok_or_else(|| {
                let message =
                    format!("{orphan_arg}: not an orphan save, as `orphan-saves` shows them");
                CliError::CouldNotStart(message)
            })
        })
        .collect()
}

/// The platform with this id, among those games can be imported for.
fn parse_platform(id: &str) -> CliResult<Platform> {
    Platform::iter()
        .find(|platform| platform.id() == id)
        .ok_or_else(|| {
            let platform_ids: Vec<String> = Platform::iter()
                .map(|platform| platform.id().to_string())
                .collect();
            let message = format!(
                "{id}: not a platform; these are: {}",
                platform_ids.join(", ")
            );
            CliError::CouldNotStart(message)
        })
}

/// Splits `<platform>/<rest>` into the platform and the rest.
fn split_platform(game_arg: &str) -> CliResult<(Platform, &str)> {
    let (id, rest) = game_arg.split_once('/').ok_or_else(|| {
        let format_hint = "write a game as <platform>/<name>, as `pocketeer games` shows it";
        CliError::CouldNotStart(format!("{game_arg}: {format_hint}"))
    })?;
    Ok((Platform::from_id(id), rest))
}

fn table<const N: usize>(header: &[&str; N], rows: &[[String; N]]) -> String {
    table_right_aligned(header, rows, N)
}

/// `rows` under `header`, in columns, with the one at index `right` aligned to
/// the right, for numbers.
fn table_right_aligned<const N: usize>(
    header: &[&str; N],
    rows: &[[String; N]],
    right: usize,
) -> String {
    let mut widths = header.map(|column_title| column_title.chars().count());
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let padded: Vec<String> = cells
            .iter()
            .zip(widths)
            .enumerate()
            .map(|(i, (cell, width))| {
                if i == right {
                    format!("{cell:>width$}")
                } else {
                    format!("{cell:<width$}")
                }
            })
            .collect();
        format!("{}\n", padded.join("  ").trim_end())
    };
    let mut table_text = line(header.to_vec());
    for row in rows {
        table_text.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    table_text
}
