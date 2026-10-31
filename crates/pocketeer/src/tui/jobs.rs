//! Background work for the TUI. Card-touching jobs run one at a time on the
//! main worker so the SD card is never written from two places; network-only
//! and read-only jobs (cover fetching, change detection, library re-reads)
//! run on a side worker and never block the UI.
//!
//! Planning and applying go through the same `commands` functions the CLI
//! uses; this file only routes a request to them and turns the result into
//! text with the shared `human` renderers.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{Receiver, Sender, TryRecvError, channel},
    },
    thread,
};

use pocketeer_core::{
    Error, Pocket, Result,
    clean::{self, CleanPlan},
    cores::{self, CoreListReport},
    install::{Action, PlacePlan, PlanOptions, Prepared, UninstallPlan},
    inventory::{Inventory, InventoryLoad},
    library::{
        Library, covers,
        db::Game,
        device::{self, Device},
        device_sync::DeviceSyncPrepared,
        import::{ImportAction, ImportOptions, ImportPlan},
        orphans::{self, CardImportPlan, CardOnlyRom},
        remove::RemovePlan as LibraryRemovePlan,
        rename::RenamePlan,
        saves::SavesPlan,
    },
    progress::Event,
    required::{self, BiosReport},
    status::{self, Status},
};

use crate::{
    cli::{DeviceImportArgs, ImportArgs, SyncArgs},
    commands::{
        Applied, Mode, Plan, Staged, apply_actions, card as card_cmd, cores as cores_cmd,
        device as device_cmd, library as library_cmd, saves as saves_cmd,
    },
    context::Context,
    human,
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
    pub clean: CleanPlan,
    /// ROM files on the card the library doesn't hold (every ROM file when
    /// there is no library). BIOS and other core-declared files are excluded.
    pub card_roms: Vec<CardOnlyRom>,
}

/// What the Games tab shows of the library.
pub struct LibraryView {
    pub root: PathBuf,
    pub games: Vec<GameRow>,
    /// Device registered on the mounted card, if any.
    pub device: Option<Device>,
    pub saves: usize,
    /// Platform ids the library holds ROMs for.
    pub platforms: Vec<String>,
}

pub struct GameRow {
    pub game: Game,
    pub cover: Option<PathBuf>,
    /// First ROM of the game exists under Assets/<platform>/common/ on the card.
    pub on_card: bool,
}

/// Something to plan on the worker; the plan comes back as a [`Pending`]
/// for the user to confirm.
pub enum Request {
    /// Sync the device registered on the mounted card.
    DeviceSync,
    Saves,
    Import(ImportArgs),
    Standardize,
    Rename {
        platform: String,
        spec: String,
        new_name: String,
    },
    CardImport(DeviceImportArgs),
}

/// A plan: shown in a dialog, then applied once confirmed.
pub enum Pending {
    Install(Vec<Prepared>),
    Remove(Vec<UninstallPlan>),
    Place(PlacePlan),
    Clean(CleanPlan),
    /// Arbitrary file actions (currently: deleting ROM files from the card).
    Files {
        label: String,
        actions: Vec<Action>,
    },
    DeviceSync(Box<DeviceSyncPrepared>),
    Saves(Box<SavesPlan>),
    Rename(Box<RenamePlan>),
    /// Renames of existing games to standard names.
    Standardize(Vec<RenamePlan>),
    LibraryRemove(Box<LibraryRemovePlan>),
    CardImport(Box<CardImportPlan>),
    /// Import from a folder on this computer.
    Import(Box<(ImportOptions, ImportPlan)>),
}

impl Pending {
    /// Imports add games one by one; the Games tab refreshes after each.
    pub fn is_import(&self) -> bool {
        matches!(self, Pending::Import(_) | Pending::CardImport(_))
    }

    /// Every (platform, current name, suggested standard name) an import
    /// plan carries; empty for anything that isn't an import.
    pub fn import_suggestions(&self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        let mut collect = |platform: &str, plan: &ImportPlan| {
            for a in &plan.actions {
                if let ImportAction::Add {
                    rom,
                    suggested: Some(s),
                    ..
                } = a
                {
                    out.push((platform.to_string(), rom.clone(), s.clone()));
                }
            }
        };
        match self {
            Pending::Import(b) => collect(&b.0.platform, &b.1),
            Pending::CardImport(c) => {
                for part in &c.parts {
                    collect(&part.platform, &part.plan);
                }
            }
            _ => {}
        }
        out
    }
}

pub enum Job {
    Load,
    Plan(Request),
    Apply(Pending),
    FetchCovers {
        platform: Option<String>,
        /// libretro system name for platforms without a built-in mapping.
        system: Option<String>,
        /// Automatic run: nothing to report when there was nothing to do.
        quiet: bool,
    },
    /// Resolve cores against the inventory and download what an install needs.
    Prepare {
        inventory: Arc<Inventory>,
        specs: Vec<String>,
    },
    /// Side worker: cheap change detection (stat only) for auto-refresh.
    Fingerprint,
    /// Side worker: re-read the library while a long import is still running.
    RefreshLibrary,
}

pub enum Msg {
    Loaded(Result<Box<Snapshot>>),
    Planned(Result<Pending>),
    Progress(Event),
    Prepared(Vec<(String, Result<Prepared>)>),
    /// What applying did, as text, and how many items failed.
    Applied {
        lines: Vec<String>,
        failed: usize,
    },
    /// Progress of the side worker (covers); doesn't block anything.
    SideProgress(Event),
    Fingerprint(u64),
    LibraryRefreshed(Result<Box<LibraryView>>),
    SideDone {
        lines: Vec<String>,
        failed: usize,
    },
}

pub struct Worker {
    tx: Sender<(Context, Job)>,
    side_tx: Sender<(Context, Job)>,
    rx: Receiver<Msg>,
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
                    Msg::Applied { lines, failed } => Msg::SideDone { lines, failed },
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
        Job::Load => Msg::Loaded(load(ctx, progress).map(Box::new)),
        Job::Plan(request) => Msg::Planned(plan(ctx, request, progress)),
        Job::Apply(pending) => apply(ctx, pending, progress),
        Job::FetchCovers {
            platform,
            system,
            quiet,
        } => fetch_covers(ctx, platform, system, quiet, progress),
        Job::Prepare { inventory, specs } => Msg::Prepared(
            cores_cmd::prepare(ctx, &inventory, &specs, PlanOptions::default(), progress)
                .unwrap_or_else(|e| vec![(String::new(), Err(e))]),
        ),
        Job::Fingerprint => Msg::Fingerprint(pocketeer_core::watch::fingerprint(
            ctx.pocket_opt().as_ref(),
            ctx.library_path().ok().as_deref(),
        )),
        Job::RefreshLibrary => Msg::LibraryRefreshed(
            ctx.library()
                .and_then(|lib| library_view(&lib, ctx.pocket_opt().as_ref()).map(Box::new)),
        ),
    }
}

fn plan(ctx: &Context, request: Request, progress: &mut dyn FnMut(Event)) -> Result<Pending> {
    Ok(match request {
        Request::DeviceSync => Pending::DeviceSync(Box::new(device_cmd::plan_sync(
            ctx,
            &SyncArgs::default(),
            progress,
        )?)),
        Request::Saves => Pending::Saves(Box::new(saves_cmd::plan_sync(
            ctx,
            saves_cmd::Direction::Both,
            true,
            progress,
        )?)),
        Request::Import(args) => {
            Pending::Import(Box::new(library_cmd::plan_import(ctx, &args, progress)?))
        }
        Request::Standardize => Pending::Standardize(library_cmd::plan_standardize(ctx, None)?),
        Request::Rename {
            platform,
            spec,
            new_name,
        } => Pending::Rename(Box::new(library_cmd::plan_rename(
            ctx,
            Some(&platform),
            &spec,
            &new_name,
        )?)),
        Request::CardImport(args) => {
            let (plan, _not_found) = device_cmd::plan_import(ctx, &args, progress)?;
            Pending::CardImport(Box::new(plan))
        }
    })
}

/// Applies a confirmed plan and renders what happened with the same text
/// the CLI prints (`Mode::Interactive`: no CLI flags in it).
fn apply(ctx: &Context, pending: Pending, progress: &mut dyn FnMut(Event)) -> Msg {
    match pending {
        Pending::Install(preps) => {
            let pocket = match ctx.pocket() {
                Ok(p) => p,
                Err(e) => return failed(e),
            };
            let prepared = preps
                .into_iter()
                .map(|p| (p.plan.core_id.clone(), Ok(p)))
                .collect();
            let report = cores_cmd::apply_prepared(&pocket, Mode::Applied, prepared, progress);
            applied(cores_cmd::install_report(&report), report.failed())
        }
        Pending::Remove(plans) => {
            let pocket = match ctx.pocket() {
                Ok(p) => p,
                Err(e) => return failed(e),
            };
            let plans = plans
                .into_iter()
                .map(|p| (p.core_id.clone(), Ok(p)))
                .collect();
            let report = cores_cmd::apply_removes(&pocket, Mode::Applied, plans, progress);
            applied(cores_cmd::remove_report(&report), report.failed())
        }
        Pending::Place(plan) => {
            let result = ctx
                .pocket()
                .and_then(|p| apply_actions(&p, "bios-place", &plan.actions, progress));
            shown(plan, result, |s| card_cmd::place_report(&s))
        }
        Pending::Clean(plan) => {
            let result = ctx.pocket().and_then(|p| clean::apply(&p, &plan));
            shown(plan, result, |s| card_cmd::clean_report(&s))
        }
        Pending::Files { label, actions } => {
            progress(Event::Status(label.clone()));
            match ctx
                .pocket()
                .and_then(|p| apply_actions(&p, &label, &actions, progress))
            {
                Ok(a) => applied(format!("✓ {label}: {} file(s) deleted", a.removed), 0),
                Err(e) => applied(format!("✗ {label}: {e}"), 1),
            }
        }
        Pending::DeviceSync(prep) => {
            let plan = prep.plan.clone();
            let result = device_cmd::apply_sync(ctx, *prep, progress);
            shown(plan, result, |s| device_cmd::device_sync(&s))
        }
        Pending::Saves(plan) => {
            let result = saves_cmd::apply_sync(ctx, &plan, progress);
            shown(*plan, result, |s| saves_cmd::saves_sync(&s))
        }
        Pending::Rename(plan) => {
            progress(Event::Status(format!("Renaming {}", plan.old_name)));
            let result = library_cmd::apply_rename(ctx, &plan);
            shown(*plan, result, |s| library_cmd::library_rename(&s))
        }
        Pending::Standardize(renames) => {
            let result = renames
                .iter()
                .map(|p| {
                    progress(Event::Status(format!("Renaming {}", p.old_name)));
                    library_cmd::apply_rename(ctx, p)
                })
                .collect();
            shown(library_cmd::StandardizePlan { renames }, result, |s| {
                library_cmd::library_standardize(&s)
            })
        }
        Pending::LibraryRemove(plan) => {
            progress(Event::Status("Removing from library".into()));
            let result = library_cmd::apply_remove(ctx, &plan);
            shown(*plan, result, |s| library_cmd::library_remove(&s))
        }
        Pending::CardImport(plan) => {
            let result = device_cmd::apply_import(ctx, &plan, progress);
            shown(*plan, result, |staged| {
                device_cmd::device_import(&device_cmd::ImportReport {
                    staged,
                    not_found: Vec::new(),
                    covers: None,
                })
            })
        }
        Pending::Import(boxed) => {
            let (options, plan) = *boxed;
            let result = library_cmd::apply_import(ctx, &options, &plan, progress);
            shown(plan, result, |staged| {
                library_cmd::library_import(&library_cmd::ImportReport {
                    staged,
                    covers: None,
                })
            })
        }
    }
}

fn applied(text: String, failed: usize) -> Msg {
    Msg::Applied {
        lines: text.lines().map(str::to_owned).collect(),
        failed,
    }
}

fn failed(e: Error) -> Msg {
    applied(format!("✗ {e}"), 1)
}

/// A plan and what applying it produced, rendered like the CLI would.
fn shown<P: Plan, R: Applied>(
    plan: P,
    result: Result<R>,
    render: impl FnOnce(Staged<P, R>) -> String,
) -> Msg {
    match result {
        Ok(r) => {
            let staged = Staged {
                mode: Mode::Interactive,
                plan,
                result: Some(r),
            };
            let failed = staged.failed();
            applied(render(staged), failed)
        }
        Err(e) => failed(e),
    }
}

fn fetch_covers(
    ctx: &Context,
    platform: Option<String>,
    system: Option<String>,
    quiet: bool,
    progress: &mut dyn FnMut(Event),
) -> Msg {
    match library_cmd::covers_fetch(ctx, platform.as_deref(), false, system.as_deref(), progress) {
        Ok(r) if quiet && r.summary.fetched == 0 && r.summary.errors == 0 => {
            applied(String::new(), 0)
        }
        Ok(r) => {
            let mut text = String::new();
            human::covers_summary(&mut text, Some(&r), Mode::Interactive);
            applied(text, 0)
        }
        Err(e) => applied(format!("✗ covers: {e}"), 1),
    }
}

/// The Games tab's view of the library: cheap enough to rebuild after every
/// imported file (database reads plus one `stat` per game on the card).
/// Without a card nothing is "on card" and no device is current.
fn library_view(lib: &Library, pocket: Option<&Pocket>) -> Result<LibraryView> {
    let games = lib.list_games(None, None)?;
    let rows = games
        .into_iter()
        .map(|game| {
            let cover = covers::path_for(lib, game.id).ok().flatten();
            let on_card = pocket.is_some_and(|pocket| {
                game.roms.first().is_some_and(|r| {
                    pocket
                        .assets_dir()
                        .join(&game.platform)
                        .join("common")
                        .join(&r.name)
                        .is_file()
                })
            });
            GameRow {
                game,
                cover,
                on_card,
            }
        })
        .collect();
    Ok(LibraryView {
        root: lib.root().to_path_buf(),
        games: rows,
        device: match pocket {
            Some(p) => device::current(lib, p)?,
            None => None,
        },
        saves: pocketeer_core::library::saves::list(lib)?.len(),
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
        pocketeer_core::watch::fingerprint(pocket.as_ref(), ctx.library_path().ok().as_deref());
    let lib = ctx.library().ok();
    let card = match &pocket {
        Some(pocket) => Some(load_card(ctx, pocket, lib.as_ref(), progress)?),
        None => None,
    };
    let library = match &lib {
        Some(lib) => {
            progress(Event::Status("Reading library".into()));
            Some(library_view(lib, pocket.as_ref())?)
        }
        None => None,
    };
    Ok(Snapshot {
        fingerprint,
        card,
        library,
    })
}

fn load_card(
    ctx: &Context,
    pocket: &Pocket,
    lib: Option<&Library>,
    progress: &mut dyn FnMut(Event),
) -> Result<CardSnapshot> {
    progress(Event::Status("Reading card".into()));
    let status = status::status(pocket)?;
    progress(Event::Status("Loading inventory".into()));
    let inventory = ctx.inventory_lenient();
    let cores = cores::report(pocket, &inventory)?;
    progress(Event::Status("Checking required files".into()));
    let installed = cores::list_installed(pocket)?;
    let bios = required::check(pocket, &installed, None)?;
    progress(Event::Status("Scanning for junk".into()));
    let clean = clean::plan(pocket, false)?;
    progress(Event::Status("Listing ROMs".into()));
    // Name and size only: nothing is read from the card here.
    let card_roms = match lib {
        Some(lib) => orphans::list(lib, pocket, None)?,
        None => orphans::scan_card(pocket, None)?,
    };
    Ok(CardSnapshot {
        status,
        cores,
        inventory: match inventory {
            InventoryLoad::Loaded(inv) => Some(Arc::new(inv)),
            InventoryLoad::Unavailable(_) => None,
        },
        bios,
        clean,
        card_roms,
    })
}
