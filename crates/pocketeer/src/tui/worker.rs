//! Background work for the TUI. Card-touching jobs run one at a time on the
//! main worker so the SD card is never written from two places; network-only
//! and read-only jobs (cover fetching, change detection, library re-reads)
//! run on a side worker and never block the UI.
//!
//! Also here: the [`Snapshot`] the screens draw from, and how it is loaded.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, Sender, TryRecvError, channel},
    },
    thread,
};

use pocketeer_core::{
    Error, Pocket, Result,
    card::Status,
    card::bios::{self, BiosReport},
    card::cores::{self, CoreList, CoreListReport},
    card::memories::Header,
    card::roms::{CardRom, Index},
    catalog::inventory::{Inventory, InventoryLoad},
    library::{Game, Library, covers},
    plan::Event,
    transfer::{
        self, LocalMatch, Presence,
        saves::{SaveOverview, Scope},
    },
};

use super::pending::{Pending, Planner, Preview, text_lines};
use crate::{
    ops::{
        context::Context,
        library as library_cmd,
        memories::{self as memories_cmd, MemoryEntry},
    },
    text::{self, Reader, Text},
};

/// Everything the screens show, loaded in one go.
pub struct Snapshot {
    /// `watch::fingerprint` at load time; a different value later means
    /// something outside this TUI changed the card or the library.
    pub fingerprint: u64,
    /// `None` while no SD card is mounted: the Games tab still works from
    /// the library alone; the other tabs wait for a card.
    pub card: Option<CardSnapshot>,
    /// `None` when there is no library at the configured path.
    pub library: Option<LibraryView>,
}

/// What the Cores, BIOS and Device tabs show of the mounted card.
pub struct CardSnapshot {
    pub status: Status,
    pub cores: CoreListReport,
    pub inventory: Option<Arc<Inventory>>,
    pub bios: BiosReport,
    /// Every ROM file on the card (BIOS and other core-declared files
    /// excluded), with whether the library holds it. A copy of the index's
    /// list, so rows can be drawn while the worker verifies the index.
    pub files: Vec<CardFile>,
    /// The ROM index, except while the worker is verifying it.
    pub index: Option<Box<Index>>,
}

/// A ROM file on the card, and whether the library has it too.
pub struct CardFile {
    pub rom: CardRom,
    pub presence: Presence,
}

/// What the Games tab shows of the library.
pub struct LibraryView {
    pub root: PathBuf,
    pub games: Vec<GameRow>,
    /// Platform ids the library holds ROMs for.
    pub platforms: Vec<String>,
}

/// Saves and Memories as the card and the library have them; read on the
/// side worker after every load, since it hashes the card's saves.
#[derive(Default)]
pub struct SavesView {
    /// Cartridge saves, by card path.
    pub saves: BTreeMap<PathBuf, SaveOverview>,
    /// The library game each linked save belongs to.
    pub owners: BTreeMap<PathBuf, i64>,
    pub memories: Vec<MemoryEntry>,
}

pub struct GameRow {
    pub game: Game,
    pub cover: Option<PathBuf>,
    pub matches: Vec<LocalMatch>,
}

impl CardSnapshot {
    /// The index is back (from loading, or from the worker): its ROMs are
    /// what the screens show. Library presence follows with `rematch`.
    pub fn set_index(&mut self, index: Box<Index>) {
        self.files = index
            .roms
            .iter()
            .map(|rom| CardFile {
                rom: rom.clone(),
                presence: Presence::Card,
            })
            .collect();
        self.index = Some(index);
    }

    pub fn installed(&self) -> CoreList {
        CoreList {
            cores: self.cores.cores.iter().map(|c| c.core.clone()).collect(),
            broken: Vec::new(),
        }
    }

    /// Required files one core is missing or has wrong.
    pub fn missing_files(&self, core_id: &str) -> Vec<&bios::RequiredFile> {
        self.bios
            .cores
            .iter()
            .filter(|c| c.core_id == core_id)
            .flat_map(|c| c.files.iter())
            .filter(|f| !matches!(f.status, bios::FileStatus::Present))
            .collect()
    }
}

impl LibraryView {
    /// The game a Memory was made with: the header names the ROM file and
    /// platform; the library may know the ROM by that name, or the card copy
    /// may carry it.
    pub fn game_of(&self, memory: &Header) -> Option<&Game> {
        let platform = memory.platform.as_deref()?;
        self.games
            .iter()
            .find(|g| {
                g.game.platform.eq_ignore_ascii_case(platform)
                    && (g
                        .game
                        .roms
                        .iter()
                        .any(|r| r.name.eq_ignore_ascii_case(&memory.game))
                        || g.matches
                            .iter()
                            .flat_map(|x| &x.copies)
                            .any(|c| c.name.eq_ignore_ascii_case(&memory.game)))
            })
            .map(|g| &g.game)
    }
}

impl GameRow {
    pub fn on_card(&self) -> bool {
        self.matches.iter().any(|m| m.presence == Presence::Both)
    }
}

pub enum Job {
    Identify,
    Verify {
        index: Box<Index>,
        cursor: usize,
        epoch: u64,
        token: Arc<AtomicU64>,
    },
    Load,
    /// Plan on the worker; the plan comes back as the dialog it opens.
    Plan(Planner),
    Apply(Box<dyn Pending>),
    FetchCovers {
        platform: Option<String>,
        /// libretro system name for platforms without a built-in mapping.
        system: Option<String>,
        /// Automatic run: nothing to report when there was nothing to do.
        quiet: bool,
    },
    /// Side worker: cheap change detection (stat only) for auto-refresh.
    Fingerprint,
    /// Side worker: re-read the library while a long import is still running.
    RefreshLibrary,
    /// Side worker: read saves and Memories. `copies` are the card paths
    /// found for each library game, so their saves can be matched.
    Saves {
        copies: BTreeMap<i64, Vec<PathBuf>>,
    },
}

pub enum Msg {
    Verified {
        result: Result<Box<Index>>,
        next: usize,
        epoch: u64,
    },
    Loaded(Result<Box<Snapshot>>),
    Planned(Result<Preview>),
    Progress(Event),
    /// A main-worker job finished.
    Applied(Outcome),
    /// Progress of the side worker (covers); doesn't block anything.
    SideProgress(Event),
    Fingerprint(u64),
    LibraryRefreshed(Result<Box<LibraryView>>),
    Saves(Result<Box<SavesView>>),
    /// A side-worker job finished.
    SideDone(Outcome),
}

/// What a finished job has to say.
pub struct Outcome {
    /// For the log; the last line is what the status bar shows.
    pub lines: Vec<String>,
    /// Items that failed; non-zero opens a dialog with `lines`.
    pub failed: usize,
    /// The card or library changed, so the screens need a reload.
    pub changed: bool,
}

pub struct Worker {
    tx: Sender<(Context, Job)>,
    side_tx: Sender<(Context, Job)>,
    rx: Receiver<Msg>,
    pub epoch: Arc<AtomicU64>,
}

impl Worker {
    pub fn spawn() -> Self {
        let (job_tx, job_rx) = channel::<(Context, Job)>();
        let (side_tx, side_rx) = channel::<(Context, Job)>();
        let (msg_tx, msg_rx) = channel::<Msg>();
        let main_tx = msg_tx.clone();
        thread::spawn(move || {
            for (ctx, job) in job_rx {
                let mut progress = |e: Event| {
                    let _ = main_tx.send(Msg::Progress(e));
                };
                let msg = run(&ctx, job, &mut progress);
                if main_tx.send(msg).is_err() {
                    break;
                }
            }
        });
        thread::spawn(move || {
            for (ctx, job) in side_rx {
                let mut progress = |e: Event| {
                    let _ = msg_tx.send(Msg::SideProgress(e));
                };
                let msg = match run(&ctx, job, &mut progress) {
                    Msg::Applied(outcome) => Msg::SideDone(outcome),
                    other => other,
                };
                if msg_tx.send(msg).is_err() {
                    break;
                }
            }
        });
        Worker {
            tx: job_tx,
            side_tx,
            rx: msg_rx,
            epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Card-touching work: serialised.
    pub fn submit(&self, ctx: Context, job: Job) {
        let _ = self.tx.send((ctx, job));
    }

    /// Network-only or read-only work that may run alongside anything else.
    pub fn submit_side(&self, ctx: Context, job: Job) {
        let _ = self.side_tx.send((ctx, job));
    }

    pub fn try_recv(&self) -> Option<Msg> {
        match self.rx.try_recv() {
            Ok(m) => Some(m),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }
}

fn run(ctx: &Context, job: Job, progress: &mut dyn FnMut(Event)) -> Msg {
    match job {
        Job::Identify => {
            let report = library_cmd::refresh_metadata(ctx, None, true, progress);
            Msg::Applied(Outcome {
                lines: text_lines(&report.text(Reader::Tui)),
                // Background work: problems go to the log, not a dialog.
                failed: 0,
                changed: report.changed(),
            })
        }
        Job::Verify {
            mut index,
            cursor,
            epoch,
            token,
        } => {
            let cancelled = || token.load(Ordering::Relaxed) != epoch;
            let mut next = cursor;
            let result = ctx.pocket().map(|pocket| {
                if index.verify(&pocket, cursor, false, &cancelled) {
                    next += 1;
                }
                if !cancelled() {
                    if let Err(e) = index.save(&pocket) {
                        index.warning = Some(e.to_string());
                    }
                }
                index
            });
            Msg::Verified {
                result,
                next,
                epoch,
            }
        }
        Job::Load => Msg::Loaded(load(ctx, progress).map(Box::new)),
        Job::Plan(planner) => Msg::Planned(planner(ctx, progress)),
        Job::Apply(pending) => pending.apply(ctx, progress),
        Job::FetchCovers {
            platform,
            system,
            quiet,
        } => fetch_covers(ctx, platform, system, quiet, progress),
        Job::Fingerprint => Msg::Fingerprint(pocketeer_core::transfer::fingerprint(
            ctx.pocket_opt().as_ref(),
            ctx.library_path().ok().as_deref(),
        )),
        Job::RefreshLibrary => Msg::LibraryRefreshed(
            ctx.library()
                .and_then(|lib| library_view(&lib).map(Box::new)),
        ),
        Job::Saves { copies } => Msg::Saves(saves_view(ctx, &copies).map(Box::new)),
    }
}

fn saves_view(ctx: &Context, copies: &BTreeMap<i64, Vec<PathBuf>>) -> Result<SavesView> {
    let lib = ctx.library()?;
    let games = lib.list_games(None, None)?;
    let owners = transfer::saves::link_owners(&lib, &games, copies)?
        .into_iter()
        .filter_map(|(id, rels)| Some((id?, rels)))
        .flat_map(|(id, rels)| rels.into_iter().map(move |r| (r, id)))
        .collect();
    let pocket = ctx.pocket_opt();
    let saves = transfer::saves::overview(&lib, pocket.as_ref(), Scope::Saves)?
        .into_iter()
        .map(|s| (s.rel_path.clone(), s))
        .collect();
    Ok(SavesView {
        saves,
        owners,
        memories: memories_cmd::list(ctx)?.memories,
    })
}

/// A job that changed the card or the library (or tried to): its text,
/// and how many items failed.
pub(super) fn applied(text: String, failed: usize) -> Msg {
    Msg::Applied(Outcome {
        lines: text_lines(&text),
        failed,
        changed: true,
    })
}

pub(super) fn failed(e: Error) -> Msg {
    applied(format!("✗ {e}"), 1)
}

fn fetch_covers(
    ctx: &Context,
    platform: Option<String>,
    system: Option<String>,
    quiet: bool,
    progress: &mut dyn FnMut(Event),
) -> Msg {
    let report = match library_cmd::covers_fetch(
        ctx,
        platform.as_ref().map(std::slice::from_ref),
        false,
        system.as_deref(),
        progress,
    ) {
        Ok(r) => r,
        Err(e) => return applied(format!("✗ covers: {e}"), 1),
    };
    let changed = report.summary.fetched > 0;
    let mut text = String::new();
    if !(quiet && !changed && report.summary.errors == 0) {
        text::covers_summary(&mut text, Some(&report), Reader::Tui);
    }
    Msg::Applied(Outcome {
        lines: text_lines(&text),
        failed: 0,
        changed,
    })
}

/// Library metadata; card presence is joined separately by content hash.
fn library_view(lib: &Library) -> Result<LibraryView> {
    let games = lib.list_games(None, None)?;
    let rows = games
        .into_iter()
        .map(|game| {
            let cover = covers::path_for(lib, game.id).ok().flatten();
            GameRow {
                game,
                cover,
                matches: Vec::new(),
            }
        })
        .collect();
    Ok(LibraryView {
        root: lib.root().to_path_buf(),
        games: rows,
        platforms: lib
            .platform_stats()?
            .into_iter()
            .map(|p| p.platform)
            .collect(),
    })
}

fn load(ctx: &Context, progress: &mut dyn FnMut(Event)) -> Result<Snapshot> {
    let pocket = ctx.pocket_opt();
    let fingerprint =
        pocketeer_core::transfer::fingerprint(pocket.as_ref(), ctx.library_path().ok().as_deref());
    let lib = match ctx.library() {
        Ok(lib) => Some(lib),
        Err(Error::LibraryNotFound { .. }) => None,
        Err(e) => return Err(e),
    };
    let card = match &pocket {
        Some(pocket) => Some(load_card(ctx, pocket, progress)?),
        None => None,
    };
    let library = match &lib {
        Some(lib) => {
            progress(Event::Status("Reading library".into()));
            Some(library_view(lib)?)
        }
        None => None,
    };
    let mut snapshot = Snapshot {
        fingerprint,
        card,
        library,
    };
    snapshot.rematch();
    Ok(snapshot)
}

fn load_card(
    ctx: &Context,
    pocket: &Pocket,
    progress: &mut dyn FnMut(Event),
) -> Result<CardSnapshot> {
    progress(Event::Status("Reading card".into()));
    let installed = cores::list_installed(pocket)?;
    let status = Status::read(pocket, &installed);
    progress(Event::Status("Loading inventory".into()));
    let inventory = ctx.inventory_lenient();
    progress(Event::Status("Checking required files".into()));
    let bios = bios::check(pocket, &installed, None)?;
    progress(Event::Status("Listing ROMs".into()));
    let declared = bios.all_files().map(|f| f.path.clone()).collect();
    let mut index = Index::scan_with_protected(pocket, &declared)?;
    // Persist removed paths even when no ROM remains to schedule verification.
    if let Err(e) = index.save(pocket) {
        index.warning = Some(format!("Cache not saved: {e}"));
    }
    let cores = installed.report(&inventory);
    let mut card = CardSnapshot {
        status,
        cores,
        inventory: match inventory {
            InventoryLoad::Loaded(inv) => Some(Arc::new(inv)),
            InventoryLoad::Unavailable(_) => None,
        },
        bios,
        files: Vec::new(),
        index: None,
    };
    card.set_index(Box::new(index));
    Ok(card)
}

impl Snapshot {
    pub fn rematch(&mut self) {
        let Some(card) = &mut self.card else {
            return;
        };
        let games = self
            .library
            .iter()
            .flat_map(|l| l.games.iter().map(|r| &r.game));
        let roms: Vec<CardRom> = card.files.iter().map(|f| f.rom.clone()).collect();
        let comparison = transfer::compare(games, &roms);
        if let Some(library) = &mut self.library {
            for row in &mut library.games {
                row.matches = row
                    .game
                    .roms
                    .iter()
                    .filter_map(|r| comparison.local.get(&r.id).cloned())
                    .collect();
            }
        }
        for (file, presence) in card.files.iter_mut().zip(comparison.card) {
            file.presence = presence;
        }
    }
}
