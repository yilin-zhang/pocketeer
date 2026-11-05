//! The command line: each command is a call or two into `Pocketeer`.

mod tui;

use std::{
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
    error::BaseError,
    library::LocalEntry,
    log::{
        FrontEnd, LogEntry, bios_ref, device_orphan_ref, device_ref, library_orphan_ref,
        library_ref,
    },
    platform::Platform,
    pocketeer::{BatchResult, GameRow},
};
use serde::Serialize;
use serde_json::{Value, json};
use strum::IntoEnumIterator;

/// Keeps a Pocket's games and saves in a library on the computer.
///
/// A game is written as `pocketeer games` shows it: `<platform>/<name>` in
/// the library, `<platform>/<path>` on the device. A command names a game
/// where its data comes from.
///
/// A device can hold several copies of a game. Only the first by path is
/// used: each command that reads the device warns of the others on stderr,
/// and a command that names one of the others is refused.
///
/// Exits with 0 when all went well, 1 when something it was for didn't, and
/// 2 when it couldn't start. With `--json` it prints one object:
/// `{"schema_version": 1, "ok": …, "data": …}`, with `"error": {"code",
/// "message"}` in place of `data` when it couldn't start. `games` also has
/// `"duplicates": [{"device", "same_game_as"}]`. A command written wrong is
/// told as text on stderr, with nothing on stdout, and exits with 2.
///
/// With no command it opens the terminal UI.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The library, a folder that must exist [default: ~/Pocketeer Library,
    /// made if there is none]
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
        place: OrphanSaves,
    },
    /// Copy from the device into the library
    Pull {
        #[command(subcommand)]
        what: Pull,
    },
    /// Copy from the library onto the device
    Push {
        #[command(subcommand)]
        what: Push,
    },
    /// List the devices mounted
    Device,
    /// Eject the device
    Eject,
    /// Update the DATs, which name games
    Dat {
        #[command(subcommand)]
        what: Dat,
    },
    /// Bring things from the computer into the library
    Import {
        #[command(subcommand)]
        what: Import,
    },
    /// Put things in the library in the system's trash
    Trash {
        #[command(subcommand)]
        what: Trash,
    },
    /// Delete from the device for good; the library's copies stay
    DeleteOnDevice {
        #[command(subcommand)]
        what: DeleteOnDevice,
    },
    /// Download covers for games in the library
    Cover {
        #[command(subcommand)]
        what: Cover,
    },
    /// The cores on the device, and those that can be installed
    Cores {
        #[command(subcommand)]
        what: Cores,
    },
    /// The files the cores on the device need, which you provide
    Bios {
        #[command(subcommand)]
        what: Bios,
    },
}

#[derive(Subcommand)]
enum Bios {
    /// List the files the cores on the device load, and which it has
    List,
    /// Copy a file of yours to where a core loads it from; the file stays, and
    /// what the device has is not replaced
    Put {
        /// Where it goes, as `bios list` shows it
        bios: String,
        /// The file to copy
        file: PathBuf,
    },
}

#[derive(Subcommand)]
enum Cores {
    /// List the cores that can be installed and those on the device
    List,
    /// Put the newest release of cores on the device
    Install {
        /// Cores, as `cores list` shows them; quote one with a space in it
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Put the newest release on the device of cores it has an older one of
    Update {
        /// Cores on the device [default: every one with an update]
        ids: Vec<String>,
    },
    /// Delete cores from the device for good; what else they use stays
    Remove {
        /// Cores on the device
        #[arg(required = true)]
        ids: Vec<String>,
    },
}

#[derive(Subcommand)]
enum Pull {
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
enum Push {
    /// Games in the library
    Game {
        /// Games in the library, as <platform>/<name>
        #[arg(required = true)]
        games: Vec<String>,
    },
    /// Saves in the library, onto their games on the device: the newest
    /// version, or the one given
    Save {
        /// Games in the library, as <platform>/<name>
        #[arg(required = true)]
        games: Vec<String>,
        /// The version to push, as `pocketeer saves` shows it
        #[arg(long, value_name = "VERSION")]
        at: Option<String>,
    },
}

#[derive(Subcommand)]
enum OrphanSaves {
    /// In the library
    Library,
    /// On the device
    Device,
}

#[derive(Subcommand)]
enum Trash {
    /// Games and their covers; their saves stay
    Game {
        /// Games in the library, as <platform>/<name>
        #[arg(required = true)]
        games: Vec<String>,
    },
    /// Saves: every version, or the one given; their games stay
    Save {
        /// Games in the library, as <platform>/<name>
        #[arg(required = true)]
        games: Vec<String>,
        /// The version to trash, as `pocketeer saves` shows it
        #[arg(long, value_name = "VERSION")]
        at: Option<String>,
    },
    /// Saves with no game: every version
    OrphanSave {
        /// As `pocketeer orphan-saves library` shows them
        #[arg(required = true)]
        saves: Vec<String>,
    },
}

#[derive(Subcommand)]
enum DeleteOnDevice {
    /// Games, even those the library doesn't have; their saves stay
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
enum Import {
    /// Games from files, each under its name in the DATs if they know it and
    /// its file name if not; the files stay
    Game {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// The platform they are for [default: what each file's extension says]
        #[arg(long)]
        platform: Option<String>,
    },
    /// A save from a file, as a version of a game's save, written when the
    /// file was; the file stays
    Save {
        /// The game in the library, as <platform>/<name>
        game: String,
        /// The save
        file: PathBuf,
    },
}

#[derive(Subcommand)]
enum Dat {
    /// Download the DATs that changed
    Sync,
}

#[derive(Subcommand)]
enum Cover {
    /// Download a cover for each game that has none
    Fetch {
        /// Games in the library, as <platform>/<name> [default: every one
        /// without a cover]
        games: Vec<String>,
    },
}

/// How to tell what a command has to say.
#[derive(Clone, Copy)]
enum PrintFormat {
    Text,
    Json,
}

/// Why a command didn't all go well.
enum CliError {
    /// It couldn't start, and this has yet to be said.
    CouldNotStart { code: &'static str, message: String },
    /// It ran and some of it failed, as it has said already.
    SomeFailed,
}

impl CliError {
    fn could_not_start(code: &'static str, message: impl Into<String>) -> Self {
        CliError::CouldNotStart {
            code,
            message: message.into(),
        }
    }
}

impl From<BaseError> for CliError {
    fn from(e: BaseError) -> Self {
        CliError::could_not_start(e.code(), e.to_string())
    }
}

type CliResult<T> = Result<T, CliError>;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Some(command) = &cli.command else {
        return terminal_ui(&cli);
    };
    let format = if cli.json {
        PrintFormat::Json
    } else {
        PrintFormat::Text
    };
    match run(&cli, command, format) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::SomeFailed) => ExitCode::from(1),
        Err(CliError::CouldNotStart { code, message }) => {
            match format {
                PrintFormat::Json => print_envelope(json!({
                    "ok": false,
                    "error": { "code": code, "message": message },
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

fn print_envelope(mut out: Value) {
    out["schema_version"] = json!(1);
    println!("{out}");
}

/// With no command, the terminal UI, if this is a terminal.
fn terminal_ui(cli: &Cli) -> ExitCode {
    if cli.json || !io::stdout().is_terminal() {
        eprintln!("error: the terminal UI needs a terminal; give a command, or see --help");
        return ExitCode::from(2);
    }
    match library_path(cli) {
        Ok(library) => tui::run(library, cli.device.clone()),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

/// The library given, or else the default one.
fn library_path(cli: &Cli) -> Result<PathBuf, BaseError> {
    match &cli.library {
        Some(library) => Ok(library.clone()),
        None => Pocketeer::default_library(),
    }
}

fn run(cli: &Cli, command: &Command, format: PrintFormat) -> CliResult<()> {
    let open = |device_use| CliSession::open(cli, device_use, format);
    match command {
        Command::Device => device(format),
        Command::Games => open(DeviceUse::IfMounted)?.games(),
        Command::Saves { game } => open(DeviceUse::None)?.saves(game),
        Command::OrphanSaves { place } => match place {
            OrphanSaves::Library => open(DeviceUse::None)?.orphan_saves_in_library(),
            OrphanSaves::Device => open(DeviceUse::Unscanned)?.orphan_saves_on_device(),
        },
        Command::Pull { what } => match what {
            Pull::Game { games } => open(DeviceUse::Scanned)?.pull_games(games),
            Pull::Save { games } => open(DeviceUse::Scanned)?.pull_saves(games),
        },
        Command::Push { what } => match what {
            Push::Game { games } => open(DeviceUse::Scanned)?.push_games(games),
            Push::Save { games, at } => open(DeviceUse::Scanned)?.push_saves(games, at.as_deref()),
        },
        Command::Import { what } => match what {
            Import::Game { files, platform } => {
                open(DeviceUse::None)?.import_games(files, platform.as_deref())
            }
            Import::Save { game, file } => open(DeviceUse::None)?.import_save(game, file),
        },
        Command::Trash { what } => match what {
            Trash::Game { games } => open(DeviceUse::None)?.trash_games(games),
            Trash::Save { games, at } => open(DeviceUse::None)?.trash_saves(games, at.as_deref()),
            Trash::OrphanSave { saves } => open(DeviceUse::None)?.trash_orphan_saves(saves),
        },
        Command::DeleteOnDevice { what } => match what {
            DeleteOnDevice::Game { games } => open(DeviceUse::Scanned)?.delete_games(games),
            DeleteOnDevice::Save { games } => open(DeviceUse::Scanned)?.delete_saves(games),
            DeleteOnDevice::OrphanSave { saves } => {
                open(DeviceUse::Unscanned)?.delete_orphan_saves(saves)
            }
        },
        Command::Cores { what } => match what {
            Cores::List => open(DeviceUse::IfMountedUnscanned)?.cores(),
            Cores::Install { ids } => open(DeviceUse::Unscanned)?.install_cores(ids),
            Cores::Update { ids } => open(DeviceUse::Unscanned)?.update_cores(ids),
            Cores::Remove { ids } => open(DeviceUse::Unscanned)?.remove_cores(ids),
        },
        Command::Bios { what } => match what {
            Bios::List => open(DeviceUse::Unscanned)?.bios(),
            Bios::Put { bios, file } => open(DeviceUse::Unscanned)?.put_bios(bios, file),
        },
        Command::Eject => open(DeviceUse::Unscanned)?.eject(),
        Command::Dat { what: Dat::Sync } => open(DeviceUse::None)?.sync_dats(),
        Command::Cover {
            what: Cover::Fetch { games },
        } => open(DeviceUse::None)?.fetch_covers(games),
    }
}

/// What a command needs of the device: opening one and above all scanning
/// it can be slow, so a command that needs neither doesn't wait for them.
enum DeviceUse {
    None,
    /// Its games if there is one; none is not an error.
    IfMounted,
    /// Its games; none is an error.
    Scanned,
    /// Its folders but not its games; none is an error.
    Unscanned,
    /// Its folders but not its games, if there is one; none is not an error.
    IfMountedUnscanned,
}

fn device(format: PrintFormat) -> CliResult<()> {
    let found: Vec<String> = Device::find()
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    match format {
        PrintFormat::Json => print_json(true, json!({ "found": found })),
        PrintFormat::Text if found.is_empty() => println!("no device mounted"),
        PrintFormat::Text => found.iter().for_each(|f| println!("{f}")),
    }
    Ok(())
}

/// What a command runs with. Commands say what they have to say themselves.
struct CliSession {
    p: Pocketeer,
    format: PrintFormat,
    /// What the command has done so far, to tell at its end.
    entries: Vec<LogEntry>,
    /// The copies the scan of the device left out.
    duplicates: Vec<DeviceEntry>,
}

impl CliSession {
    /// The library, and the device given or the one mounted as `device_use`
    /// says.
    fn open(cli: &Cli, device_use: DeviceUse, format: PrintFormat) -> CliResult<Self> {
        let mut p = Pocketeer::open(&library_path(cli)?, FrontEnd::Cli)?;
        let mut duplicates = Vec::new();
        if !matches!(device_use, DeviceUse::None) {
            let opened = match &cli.device {
                Some(device) => p.open_device(device).map(|()| true)?,
                None => p.open_mounted_device()?,
            };
            match (opened, device_use) {
                (false, DeviceUse::IfMounted | DeviceUse::IfMountedUnscanned)
                | (true, DeviceUse::Unscanned | DeviceUse::IfMountedUnscanned) => {}
                (false, _) => {
                    return Err(CliError::could_not_start(
                        "no_device",
                        "no device found; give one with --device",
                    ));
                }
                (true, _) => duplicates = p.scan_device()?.duplicates,
            }
        }
        let session = Self {
            p,
            format,
            entries: Vec::new(),
            duplicates,
        };
        for duplicate in &session.duplicates {
            eprintln!(
                "warning: {} is the same game as {}; it is left out and its save isn't synced",
                device_ref(duplicate),
                device_ref(session.first_copy(duplicate)),
            );
        }
        Ok(session)
    }

    /// The copy of the game the scan kept in place of `duplicate`.
    fn first_copy(&self, duplicate: &DeviceEntry) -> &DeviceEntry {
        let device = self.p.device().expect("the device is open");
        let first = device.entry_for(&duplicate.game);
        first.expect("the scan keeps a copy of each game")
    }

    fn games(&self) -> CliResult<()> {
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
            .p
            .games()
            .iter()
            .map(|r| {
                let id = r.game().platform.id();
                let pulls_as = match r {
                    GameRow::OnDevice(d) => Some(format!("{id}/{}", self.p.pull_name(d))),
                    _ => None,
                };
                Row {
                    platform: id.to_string(),
                    library: r.local().map(library_ref),
                    device: r.on_device().map(device_ref),
                    saves_kept: r.saves_kept(),
                    new_save_on_device: r.new_save_on_device(),
                    pulls_as,
                    cover: r.local().and_then(|l| l.cover_path.clone()),
                }
            })
            .collect();
        match self.format {
            PrintFormat::Json => {
                let duplicates: Vec<Value> = self
                    .duplicates
                    .iter()
                    .map(|d| {
                        json!({
                            "device": device_ref(d),
                            "same_game_as": device_ref(self.first_copy(d)),
                        })
                    })
                    .collect();
                print_envelope(json!({ "ok": true, "data": rows, "duplicates": duplicates }));
            }
            PrintFormat::Text => {
                let cells: Vec<[String; 4]> = rows
                    .iter()
                    .map(|r| {
                        let save = match (r.saves_kept, r.new_save_on_device) {
                            (0, false) => "-".to_string(),
                            (0, true) => "new on the device".to_string(),
                            (n, false) => format!("{n} kept"),
                            (n, true) => format!("{n} kept, new on the device"),
                        };
                        let or_dash = |s: Option<&String>| s.cloned().unwrap_or_else(|| "-".into());
                        let pulls_as = r.pulls_as.clone().unwrap_or_default();
                        [
                            or_dash(r.library.as_ref()),
                            or_dash(r.device.as_ref()),
                            save,
                            pulls_as,
                        ]
                    })
                    .collect();
                if self.p.device().is_none() {
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

    fn saves(&self, game: &str) -> CliResult<()> {
        #[derive(Serialize)]
        struct Version {
            version: String,
            size: u64,
            newest: bool,
        }
        let history = self.local(game)?.save.as_ref();
        let versions: Vec<Version> = history
            .into_iter()
            .flat_map(|h| h.iter())
            .enumerate()
            .map(|(i, s)| Version {
                version: s.timestamp(),
                size: s.size,
                newest: i == 0,
            })
            .collect();
        match self.format {
            PrintFormat::Json => print_json(true, json!(versions)),
            PrintFormat::Text if versions.is_empty() => println!("no save in the library"),
            PrintFormat::Text => {
                let cells: Vec<[String; 3]> = versions
                    .iter()
                    .map(|v| {
                        let note = if v.newest { "newest" } else { "" };
                        [v.version.clone(), format!("{} bytes", v.size), note.into()]
                    })
                    .collect();
                print!("{}", table_right(&["VERSION", "SIZE", ""], &cells, 1));
            }
        }
        Ok(())
    }

    fn orphan_saves_in_library(&self) -> CliResult<()> {
        #[derive(Serialize)]
        struct Orphan {
            save: String,
            versions: usize,
            newest: String,
        }
        let orphans: Vec<Orphan> = self
            .p
            .orphan_saves_in_library()?
            .into_iter()
            .map(|o| Orphan {
                save: library_orphan_ref(&o),
                versions: o.save.iter().count(),
                newest: o.save.current.timestamp(),
            })
            .collect();
        match self.format {
            PrintFormat::Json => print_json(true, json!(orphans)),
            PrintFormat::Text if orphans.is_empty() => println!("no orphan saves"),
            PrintFormat::Text => {
                let cells: Vec<[String; 3]> = orphans
                    .iter()
                    .map(|o| [o.save.clone(), o.versions.to_string(), o.newest.clone()])
                    .collect();
                print!("{}", table(&["SAVE", "VERSIONS", "NEWEST"], &cells));
            }
        }
        Ok(())
    }

    fn orphan_saves_on_device(&self) -> CliResult<()> {
        #[derive(Serialize)]
        struct Orphan {
            save: String,
            size: u64,
            written: String,
        }
        let orphans: Vec<Orphan> = self
            .p
            .orphan_saves_on_device()?
            .into_iter()
            .map(|o| Orphan {
                save: device_orphan_ref(&o),
                size: o.save.size,
                written: o.save.timestamp(),
            })
            .collect();
        match self.format {
            PrintFormat::Json => print_json(true, json!(orphans)),
            PrintFormat::Text if orphans.is_empty() => println!("no orphan saves"),
            PrintFormat::Text => {
                let cells: Vec<[String; 3]> = orphans
                    .iter()
                    .map(|o| {
                        [
                            o.save.clone(),
                            format!("{} bytes", o.size),
                            o.written.clone(),
                        ]
                    })
                    .collect();
                print!("{}", table_right(&["SAVE", "SIZE", "WRITTEN"], &cells, 1));
            }
        }
        Ok(())
    }

    fn pull_games(&mut self, refs: &[String]) -> CliResult<()> {
        let games = match refs {
            [] => self.p.games_only_on_device(),
            refs => self.on_device(refs)?,
        };
        self.batch(|p, each| p.pull_games(&games, each))
    }

    fn pull_saves(&mut self, refs: &[String]) -> CliResult<()> {
        let games = match refs {
            [] => self.games_with_saves_to_pull(),
            refs => self.on_device(refs)?,
        };
        self.batch(|p, each| p.pull_saves(&games, each))
    }

    /// The games on the device the library has: nobody asked for the others
    /// by name, so they aren't failures, but saves only they have are told of.
    fn games_with_saves_to_pull(&self) -> Vec<DeviceEntry> {
        let others = self.p.games_only_on_device();
        let unsaved: Vec<String> = others
            .iter()
            .filter(|g| g.save.is_some())
            .map(device_ref)
            .collect();
        if !unsaved.is_empty() {
            eprintln!(
                "warning: these games on the device have saves but aren't in the library, so their saves weren't backed up: {}",
                unsaved.join(", ")
            );
        }
        self.p.games_on_device_in_library()
    }

    fn push_games(&mut self, refs: &[String]) -> CliResult<()> {
        let games = self.in_library(refs)?;
        self.batch(|p, each| p.push_games(&games, each))
    }

    fn push_saves(&mut self, refs: &[String], at: Option<&str>) -> CliResult<()> {
        let games = self.in_library(refs)?;
        let device = self.p.device().expect("the device is open");
        let copies = refs
            .iter()
            .zip(&games)
            .map(|(r, game)| {
                let copy = device.entry_for(&game.game).cloned();
                copy.ok_or_else(|| not_on_device(r))
            })
            .collect::<CliResult<Vec<DeviceEntry>>>()?;
        self.batch(|p, each| match at {
            Some(at) => p.push_saves_at(&copies, at, each),
            None => p.push_saves(&copies, each),
        })
    }

    fn import_games(&mut self, files: &[PathBuf], platform: Option<&str>) -> CliResult<()> {
        let platform = platform.map(named_platform).transpose()?;
        self.batch(|p, each| p.import_games(files, platform.as_ref(), each))
    }

    fn import_save(&mut self, game: &str, file: &Path) -> CliResult<()> {
        let saves = [(self.local(game)?.clone(), file.to_path_buf())];
        self.batch(|p, each| p.import_saves(&saves, each))
    }

    fn trash_games(&mut self, refs: &[String]) -> CliResult<()> {
        let games = self.in_library(refs)?;
        self.batch(|p, each| p.trash_games_in_library(&games, each))
    }

    fn trash_saves(&mut self, refs: &[String], at: Option<&str>) -> CliResult<()> {
        let games = self.in_library(refs)?;
        self.batch(|p, each| match at {
            Some(at) => p.trash_saves_at(&games, at, each),
            None => p.trash_saves_in_library(&games, each),
        })
    }

    fn trash_orphan_saves(&mut self, refs: &[String]) -> CliResult<()> {
        let orphans = self.p.orphan_saves_in_library()?;
        let orphans = picked(refs, &orphans, library_orphan_ref)?;
        self.batch(|p, each| p.trash_orphan_saves_in_library(&orphans, each))
    }

    fn delete_games(&mut self, refs: &[String]) -> CliResult<()> {
        let games = self.on_device(refs)?;
        self.batch(|p, each| p.delete_games_on_device(&games, each))
    }

    fn delete_saves(&mut self, refs: &[String]) -> CliResult<()> {
        let games = self.on_device(refs)?;
        self.batch(|p, each| p.delete_saves_on_device(&games, each))
    }

    fn delete_orphan_saves(&mut self, refs: &[String]) -> CliResult<()> {
        let orphans = self.p.orphan_saves_on_device()?;
        let orphans = picked(refs, &orphans, device_orphan_ref)?;
        self.batch(|p, each| p.delete_orphan_saves_on_device(&orphans, each))
    }

    fn fetch_covers(&mut self, refs: &[String]) -> CliResult<()> {
        let games = match refs {
            [] => self.p.games_without_covers(),
            refs => self.in_library(refs)?,
        };
        self.batch(|p, each| p.fetch_covers(&games, each))
    }

    /// Runs a command going through games. As text each is printed as it is
    /// done, since a device is slow and that shows that something is happening;
    /// as JSON they all are at the end.
    fn batch(
        &mut self,
        run: impl FnOnce(
            &mut Pocketeer,
            &mut dyn FnMut(&Pocketeer, LogEntry) -> ControlFlow<()>,
        ) -> BatchResult,
    ) -> CliResult<()> {
        let (format, entries) = (self.format, &mut self.entries);
        let done = run(&mut self.p, &mut |_, entry| {
            if matches!(format, PrintFormat::Text) {
                println!("{}", entry.event.line());
            }
            entries.push(entry);
            ControlFlow::Continue(())
        });
        self.finish_batch(done)
    }

    /// The end of a command going through games, which `done` says if it
    /// stopped.
    fn finish_batch(&self, done: BatchResult) -> CliResult<()> {
        if let Err(e) = &done {
            eprintln!("error: {e}");
        }
        let ok = done.is_ok() && self.entries.iter().all(|e| e.event.ok());
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

    fn cores(&mut self) -> CliResult<()> {
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
        let (rows, failure) = self.p.core_rows()?;
        if let Some(failure) = failure {
            eprintln!("warning: couldn't get the cores that can be installed: {failure}");
        }
        let rows: Vec<Row> = rows
            .iter()
            .map(|r: &CoreRow| {
                let platforms = match (r.latest(), r.installed()) {
                    (Some(latest), _) => &latest.platforms,
                    (None, Some(installed)) => &installed.platforms,
                    (None, None) => unreachable!("a core is on one side or the other"),
                };
                Row {
                    id: r.id().to_string(),
                    platforms: platforms.iter().map(|p| p.id().to_string()).collect(),
                    installed: r.installed().is_some(),
                    version: r.installed().and_then(|i| i.version.clone()),
                    latest: r.latest().map(|l| l.version.clone()),
                    update_available: r.update_available(),
                }
            })
            .collect();
        match self.format {
            PrintFormat::Json => print_json(true, json!(rows)),
            PrintFormat::Text => {
                let cells: Vec<[String; 4]> = rows
                    .iter()
                    .map(|r| {
                        let on_device = match (r.installed, &r.version) {
                            (false, _) => "-".to_string(),
                            (true, Some(version)) => version.clone(),
                            (true, None) => "unknown version".to_string(),
                        };
                        let latest = r.latest.clone().unwrap_or_else(|| "-".to_string());
                        [r.id.clone(), r.platforms.join(", "), on_device, latest]
                    })
                    .collect();
                if self.p.device().is_none() {
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

    fn install_cores(&mut self, ids: &[String]) -> CliResult<()> {
        if let Some(id) = ids.iter().find(|id| !SUPPORTED.contains(&id.as_str())) {
            let message = format!("{id}: not a core that can be installed, as `cores list` shows");
            return Err(CliError::could_not_start("unsupported_core", message));
        }
        self.batch(|p, each| p.install_cores(ids, each))
    }

    fn update_cores(&mut self, ids: &[String]) -> CliResult<()> {
        let ids = match ids {
            [] => self.p.cores_with_updates()?,
            ids => {
                self.check_on_device(ids)?;
                ids.to_vec()
            }
        };
        if ids.is_empty() && matches!(self.format, PrintFormat::Text) {
            println!("every core is up to date");
        }
        self.batch(|p, each| p.install_cores(&ids, each))
    }

    fn remove_cores(&mut self, ids: &[String]) -> CliResult<()> {
        self.check_on_device(ids)?;
        self.batch(|p, each| p.remove_cores(ids, each))
    }

    /// Refuses cores the device doesn't have.
    fn check_on_device(&self, ids: &[String]) -> CliResult<()> {
        let device = self.p.device().expect("the device is open");
        let cores = device.cores()?;
        match ids.iter().find(|id| !cores.iter().any(|c| c.id == **id)) {
            Some(id) => Err(CliError::could_not_start(
                "core_not_on_device",
                format!("{id}: not on the device"),
            )),
            None => Ok(()),
        }
    }

    fn bios(&self) -> CliResult<()> {
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
            .p
            .bios_rows()?
            .iter()
            .map(|r| {
                let (status, size) = match r.status {
                    BiosStatus::Present => ("present", None),
                    BiosStatus::WrongSize { size } => ("wrong_size", Some(size)),
                    BiosStatus::Missing => ("missing", None),
                };
                let needed_by = r.needed_by.iter().map(|(core, required)| NeededBy {
                    core: core.clone(),
                    required: *required,
                });
                Row {
                    file: bios_ref(r),
                    status,
                    size,
                    size_exact: r.size_exact,
                    required: r.required(),
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
                    .map(|r| {
                        let status = match (r.status, r.size, r.size_exact) {
                            ("wrong_size", Some(size), Some(exact)) => {
                                format!("wrong size: {size} bytes, not {exact}")
                            }
                            ("missing", ..) if !r.required => "missing, optional".to_string(),
                            (status, ..) => status.to_string(),
                        };
                        let cores: Vec<&str> =
                            r.needed_by.iter().map(|n| n.core.as_str()).collect();
                        [r.file.clone(), status, cores.join(", ")]
                    })
                    .collect();
                print!("{}", table(&["FILE", "STATUS", "NEEDED BY"], &cells));
            }
        }
        Ok(())
    }

    fn put_bios(&mut self, file: &str, from: &Path) -> CliResult<()> {
        let rows = self.p.bios_rows()?;
        let Some(row) = rows.into_iter().find(|r| bios_ref(r) == file) else {
            let message = format!("{file}: no core on the device loads it, as `bios list` shows");
            return Err(CliError::could_not_start("not_a_bios_file", message));
        };
        let files = [(row, from.to_path_buf())];
        self.batch(|p, each| p.put_bios(&files, each))
    }

    fn eject(&mut self) -> CliResult<()> {
        let root = self.p.device().map(|d| d.root().to_path_buf());
        self.p.eject_device()?;
        match self.format {
            PrintFormat::Json => {
                let root = root.map(|r| r.display().to_string());
                print_json(true, json!({ "ejected": root }));
            }
            PrintFormat::Text => println!("ejected"),
        }
        Ok(())
    }

    fn sync_dats(&mut self) -> CliResult<()> {
        let report = self.p.sync_dats()?;
        let ids = |ps: &[Platform]| ps.iter().map(|p| p.id().to_string()).collect::<Vec<_>>();
        let (updated, missing) = (ids(&report.updated), ids(&report.missing));
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

    /// The device's games written in `refs`. A copy the scan left out is
    /// refused: what is asked of a file must not be done to another.
    fn on_device(&self, refs: &[String]) -> CliResult<Vec<DeviceEntry>> {
        let device = self.p.device().expect("the device is open");
        refs.iter()
            .map(|r| {
                let (platform, path) = split(r)?;
                let path = Path::new(path);
                let is_it = |d: &&DeviceEntry| d.game.platform == platform && d.path == path;
                if let Some(duplicate) = self.duplicates.iter().find(is_it) {
                    let first = device_ref(self.first_copy(duplicate));
                    let message = format!("{r}: the same game as {first}, the copy that is used");
                    return Err(CliError::could_not_start("duplicate_on_device", message));
                }
                let found = device.entry(&platform, path);
                found.cloned().ok_or_else(|| not_on_device(r))
            })
            .collect()
    }

    /// The library's games written in `refs`.
    fn in_library(&self, refs: &[String]) -> CliResult<Vec<LocalEntry>> {
        refs.iter().map(|r| self.local(r).cloned()).collect()
    }

    fn local(&self, r: &str) -> CliResult<&LocalEntry> {
        let (platform, name) = split(r)?;
        let not_there =
            || CliError::could_not_start("not_in_library", format!("{r}: not in the library"));
        self.p
            .library()
            .entry(&platform, name)
            .ok_or_else(not_there)
    }
}

fn not_on_device(r: &str) -> CliError {
    CliError::could_not_start("not_on_device", format!("{r}: not on the device"))
}

/// The orphan saves written in `refs`, as `orphan-saves` shows them: they name
/// nothing else, so no save of a game is taken for one.
fn picked<T: Clone>(refs: &[String], orphans: &[T], ref_of: fn(&T) -> String) -> CliResult<Vec<T>> {
    refs.iter()
        .map(|r| {
            let found = orphans.iter().find(|o| ref_of(o) == *r).cloned();
            found.ok_or_else(|| {
                let message = format!("{r}: not an orphan save, as `orphan-saves` shows them");
                CliError::could_not_start("not_an_orphan_save", message)
            })
        })
        .collect()
}

/// The platform with this id, of those games can be imported for.
fn named_platform(id: &str) -> CliResult<Platform> {
    Platform::iter().find(|p| p.id() == id).ok_or_else(|| {
        let ids: Vec<String> = Platform::iter().map(|p| p.id().to_string()).collect();
        let message = format!("{id}: not a platform; these are: {}", ids.join(", "));
        CliError::could_not_start("unknown_platform", message)
    })
}

/// `<platform>/<rest>` as the platform and the rest.
fn split(r: &str) -> CliResult<(Platform, &str)> {
    let (id, rest) = r.split_once('/').ok_or_else(|| {
        let how = "write a game as <platform>/<name>, as `pocketeer games` shows it";
        CliError::could_not_start("bad_game", format!("{r}: {how}"))
    })?;
    Ok((Platform::from_id(id), rest))
}

/// `rows` under `header`, in columns.
fn table<const N: usize>(header: &[&str; N], rows: &[[String; N]]) -> String {
    table_right(header, rows, N)
}

/// `rows` under `header`, in columns, with the one at index `right` aligned to
/// the right, for numbers.
fn table_right<const N: usize>(header: &[&str; N], rows: &[[String; N]], right: usize) -> String {
    let mut widths = header.map(|h| h.chars().count());
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let padded: Vec<String> = cells
            .iter()
            .zip(widths)
            .enumerate()
            .map(|(i, (c, w))| {
                if i == right {
                    format!("{c:>w$}")
                } else {
                    format!("{c:<w$}")
                }
            })
            .collect();
        format!("{}\n", padded.join("  ").trim_end())
    };
    let mut out = line(header.to_vec());
    for row in rows {
        out.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    out
}
