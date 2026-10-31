use std::{path::PathBuf, process::ExitCode};

use pocketeer_core::{
    Error, Result,
    library::{
        self, Library,
        covers::{self, CoverStats, FetchOptions, FetchReport},
        db::Game,
        import::{self, ImportMode, ImportOptions, ImportPlan, ImportResult},
        remove::{self, RemovePlan, RemoveResult},
        rename::{self, RenamePlan, RenameResult},
    },
    progress::Event,
};
use serde::Serialize;

use crate::commands::Mode;
use crate::human::{
    bytes, covers_summary, import_actions, plan_lines, skipped_summary, staged_footer,
    suggestion_footer, table,
};
use crate::{
    cli::{ApplyArgs, CoversCommand, ImportArgs, ImportModeArg, LibraryCommand},
    commands::{Applied, Plan, Staged, load_dat, stage},
    context::Context,
    output::{Outcome, Output},
};
use pocketeer_core::{
    library::{Status as LibraryStatus, covers::CoverStatus},
    roms::rename::RenameKind,
};
use std::fmt::Write;

pub fn run(
    ctx: &Context,
    out: &Output,
    cmd: LibraryCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        LibraryCommand::Init => out.emit(init(ctx), library_init),
        LibraryCommand::Status => out.emit(status(ctx), library_status),
        LibraryCommand::Import(args) => {
            out.emit_outcome(import(ctx, &args, progress), library_import)
        }
        LibraryCommand::List { platform, query } => out.emit(
            list(ctx, platform.as_deref(), query.as_deref()),
            library_list,
        ),
        LibraryCommand::Rename {
            rom,
            new_name,
            platform,
            apply,
        } => out.emit_outcome(
            rename(ctx, platform.as_deref(), &rom, &new_name, apply),
            library_rename,
        ),
        LibraryCommand::Remove {
            roms,
            platform,
            apply,
        } => out.emit_outcome(
            remove(ctx, platform.as_deref(), &roms, apply),
            library_remove,
        ),
        LibraryCommand::Standardize { platform, apply } => out.emit_outcome(
            standardize(ctx, platform.as_deref(), apply),
            library_standardize,
        ),
    }
}

impl From<ImportModeArg> for ImportMode {
    fn from(m: ImportModeArg) -> Self {
        match m {
            ImportModeArg::Copy => ImportMode::Copy,
            ImportModeArg::Reference => ImportMode::Reference,
            ImportModeArg::Link => ImportMode::Link,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct InitReport {
    pub root: PathBuf,
    pub created: bool,
}

pub fn init(ctx: &Context) -> Result<InitReport> {
    let root = ctx.library_path()?;
    Library::init(&root)?;
    Ok(InitReport {
        root,
        created: true,
    })
}

pub fn status(ctx: &Context) -> Result<library::Status> {
    ctx.library()?.status()
}

#[derive(Debug, Serialize)]
pub struct ImportReport {
    #[serde(flatten)]
    pub staged: Staged<ImportPlan, ImportResult>,
    /// Covers fetched for the imported platform afterwards (online only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covers: Option<FetchReport>,
}

impl ImportReport {
    /// A plan shown in a TUI dialog before it is applied.
    pub fn preview(plan: ImportPlan) -> Self {
        ImportReport {
            staged: Staged::preview(plan),
            covers: None,
        }
    }
}

impl Outcome for ImportReport {
    fn error(&self) -> Option<Error> {
        self.staged.error()
    }
}

/// Best-effort cover fetch once an import added `added` games: nothing
/// happens when it added none, when `--no-covers` was given or offline, and
/// errors are swallowed (the import already succeeded).
pub fn covers_after_import(
    ctx: &Context,
    lib: &Library,
    platform: Option<&str>,
    (added, enabled): (usize, bool),
    progress: &mut dyn FnMut(Event),
) -> Option<FetchReport> {
    if added == 0 || !enabled || ctx.offline() {
        return None;
    }
    let http = ctx.http().ok()?;
    covers::fetch(
        lib,
        &http,
        &FetchOptions {
            platform: platform.map(str::to_owned),
            retry_missing: false,
            system: None,
        },
        progress,
    )
    .ok()
}

/// Standard-name list for a platform (online only; `None` means no suggestions).
pub fn names_for(ctx: &Context, platform: &str) -> Option<Vec<String>> {
    if ctx.offline() {
        return None;
    }
    let net = ctx.net().ok()?;
    library::names::for_platform(&net.http, &net.cache, platform, ctx.policy())
}

/// Marks each unverified file in `plan` with the standard name it matches;
/// with `take` (`--standard-names`) the plan imports under those names.
pub fn suggest_standard_names(
    ctx: &Context,
    lib: &Library,
    platform: &str,
    plan: &mut ImportPlan,
    take: bool,
) -> Result<()> {
    if let Some(names) = names_for(ctx, platform)
        && import::suggest_names(plan, &names) > 0
        && take
    {
        import::apply_suggestions(lib, plan, |_| true)?;
    }
    Ok(())
}

/// Plans an import: what would be added, skipped, and (when online) which
/// files have a standard name to suggest.
pub fn plan_import(
    ctx: &Context,
    args: &ImportArgs,
    progress: &mut dyn FnMut(Event),
) -> Result<(ImportOptions, ImportPlan)> {
    let lib = ctx.library()?;
    let dat = load_dat(args.dat.as_deref())?;
    let opts = ImportOptions {
        platform: args.platform.clone(),
        platform_name: ctx.platform_name(&args.platform),
        mode: args.mode.into(),
        dat_only: args.dat_only,
    };
    let mut plan = import::plan_import(
        &lib,
        &args.dir,
        &opts,
        dat.as_ref(),
        &mut ctx.hash_cache(),
        progress,
    )?;
    suggest_standard_names(ctx, &lib, &args.platform, &mut plan, args.standard_names)?;
    Ok((opts, plan))
}

pub fn apply_import(
    ctx: &Context,
    options: &ImportOptions,
    plan: &ImportPlan,
    progress: &mut dyn FnMut(Event),
) -> Result<ImportResult> {
    import::apply_import(&mut ctx.library()?, options, plan, progress)
}

pub fn import(
    ctx: &Context,
    args: &ImportArgs,
    progress: &mut dyn FnMut(Event),
) -> Result<ImportReport> {
    let (opts, plan) = plan_import(ctx, args, progress)?;
    let staged = stage(args.apply, plan, |plan| {
        apply_import(ctx, &opts, plan, progress)
    })?;
    let added = staged.result.as_ref().map_or(0, |r| r.added);
    let covers = covers_after_import(
        ctx,
        &ctx.library()?,
        Some(&args.platform),
        (added, !args.no_covers),
        progress,
    );
    Ok(ImportReport { staged, covers })
}

#[derive(Debug, Serialize)]
pub struct ListReport {
    pub games: Vec<Game>,
    pub roms: usize,
    pub bytes: u64,
}

pub fn list(ctx: &Context, platform: Option<&str>, query: Option<&str>) -> Result<ListReport> {
    let lib = ctx.library()?;
    let games = lib.list_games(platform, query)?;
    Ok(ListReport {
        roms: games.iter().map(|g| g.roms.len()).sum(),
        bytes: games
            .iter()
            .flat_map(|g| g.roms.iter())
            .map(|r| r.size)
            .sum(),
        games,
    })
}

pub type RenameReport = Staged<RenamePlan, RenameResult>;

/// The card is optional: without one, only the library changes and the next
/// device sync renames the card's copy by name.
pub fn plan_rename(
    ctx: &Context,
    platform: Option<&str>,
    spec: &str,
    new_name: &str,
) -> Result<RenamePlan> {
    rename::plan(
        &ctx.library()?,
        ctx.pocket_opt().as_ref(),
        platform,
        spec,
        new_name,
    )
}

pub fn apply_rename(ctx: &Context, plan: &RenamePlan) -> Result<RenameResult> {
    rename::apply(&mut ctx.library()?, ctx.pocket_opt().as_ref(), plan)
}

pub fn rename(
    ctx: &Context,
    platform: Option<&str>,
    spec: &str,
    new_name: &str,
    apply: ApplyArgs,
) -> Result<RenameReport> {
    let plan = plan_rename(ctx, platform, spec, new_name)?;
    stage(apply, plan, |plan| apply_rename(ctx, plan))
}

pub type RemoveReport = Staged<RemovePlan, RemoveResult>;

pub fn plan_remove(ctx: &Context, platform: Option<&str>, specs: &[String]) -> Result<RemovePlan> {
    remove::plan(&ctx.library()?, platform, specs)
}

pub fn apply_remove(ctx: &Context, plan: &RemovePlan) -> Result<RemoveResult> {
    remove::apply(&mut ctx.library()?, plan)
}

pub fn remove(
    ctx: &Context,
    platform: Option<&str>,
    specs: &[String],
    apply: ApplyArgs,
) -> Result<RemoveReport> {
    let plan = plan_remove(ctx, platform, specs)?;
    stage(apply, plan, |plan| apply_remove(ctx, plan))
}

/// One rename per game that matches a standard name.
#[derive(Debug, Serialize)]
pub struct StandardizePlan {
    pub renames: Vec<RenamePlan>,
}

impl Plan for StandardizePlan {
    fn changes(&self) -> usize {
        self.renames.len()
    }
}

/// `result` of a standardize: one entry per rename, in plan order.
impl Applied for Vec<RenameResult> {
    fn failed(&self) -> usize {
        self.iter().map(|r| r.failed.len()).sum()
    }
}

pub type StandardizeReport = Staged<StandardizePlan, Vec<RenameResult>>;

/// Every unverified game that matches a standard name, as a rename plan
/// (card copies and saves follow, as with `library rename`).
pub fn plan_standardize(ctx: &Context, platform: Option<&str>) -> Result<Vec<RenamePlan>> {
    library::names::standardize_plan(&ctx.library()?, ctx.pocket_opt().as_ref(), platform, |p| {
        names_for(ctx, p)
    })
}

pub fn standardize(
    ctx: &Context,
    platform: Option<&str>,
    apply: ApplyArgs,
) -> Result<StandardizeReport> {
    let renames = plan_standardize(ctx, platform)?;
    stage(apply, StandardizePlan { renames }, |plan| {
        plan.renames.iter().map(|p| apply_rename(ctx, p)).collect()
    })
}

// ---------------------------------------------------------------------------
// covers: box art for library games, from libretro-thumbnails.

pub fn covers(
    ctx: &Context,
    out: &Output,
    cmd: CoversCommand,
    progress: &mut dyn FnMut(Event),
) -> ExitCode {
    match cmd {
        CoversCommand::Fetch {
            platform,
            retry,
            libretro_system,
        } => out.emit(
            covers_fetch(
                ctx,
                platform.as_deref(),
                retry,
                libretro_system.as_deref(),
                progress,
            ),
            covers_fetch_text,
        ),
        CoversCommand::Status => out.emit(covers_status(ctx), covers_status_text),
    }
}

pub fn covers_fetch(
    ctx: &Context,
    platform: Option<&str>,
    retry: bool,
    system: Option<&str>,
    progress: &mut dyn FnMut(Event),
) -> Result<FetchReport> {
    let lib = ctx.library()?;
    let http = ctx.http()?;
    let opts = FetchOptions {
        platform: platform.map(str::to_owned),
        retry_missing: retry,
        system: system.map(str::to_owned),
    };
    // Undocumented: lets tests (or a mirror) replace the libretro host.
    match std::env::var("POCKETEER_THUMBNAILS_BASE") {
        Ok(base) if !base.is_empty() => {
            covers::fetch_with_base(&lib, &http, &opts, &base, progress)
        }
        _ => covers::fetch(&lib, &http, &opts, progress),
    }
}

#[derive(Debug, Serialize)]
pub struct CoversStatusReport {
    pub platforms: Vec<CoverStats>,
}

pub fn covers_status(ctx: &Context) -> Result<CoversStatusReport> {
    Ok(CoversStatusReport {
        platforms: covers::stats(&ctx.library()?)?,
    })
}

// ---------------------------------------------------------------------------
// human-readable output

pub fn library_init(r: &InitReport) -> String {
    format!(
        "Created library at {}\nNext: pocketeer library import <folder> --platform <id> [--dat <file>]\n",
        r.root.display()
    )
}

pub fn library_status(s: &LibraryStatus) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Library    {}", s.root.display());
    let _ = writeln!(
        out,
        "Contents   {} game(s), {} ROM(s), {}",
        s.games,
        s.roms,
        bytes(s.bytes)
    );
    if !s.platforms.is_empty() {
        let _ = writeln!(out);
        let rows: Vec<[String; 5]> = s
            .platforms
            .iter()
            .map(|p| {
                [
                    p.platform.clone(),
                    p.name.clone().unwrap_or_default(),
                    p.games.to_string(),
                    format!("{}/{}", p.verified, p.roms),
                    bytes(p.bytes),
                ]
            })
            .collect();
        table(
            &mut out,
            ["PLATFORM", "NAME", "GAMES", "VERIFIED", "SIZE"],
            &rows,
        );
    }
    out
}

pub fn library_import(r: &ImportReport) -> String {
    let mut out = String::new();
    let p = &r.staged.plan;
    let _ = writeln!(
        out,
        "{}  →  library/{} ({})",
        p.dir.display(),
        p.platform,
        match p.mode {
            ImportMode::Copy => "copy",
            ImportMode::Reference => "reference in place",
            ImportMode::Link => "hard link",
        }
    );
    let skipped = import_actions(&mut out, p.actions.iter());
    let s = &p.summary;
    let _ = writeln!(
        out,
        "{} to add ({} verified, {} unverified, {}){}",
        s.add,
        s.verified,
        s.unverified,
        bytes(s.bytes),
        skipped_summary(&skipped)
    );
    if r.staged.result.is_none() && p.changes() > 0 {
        suggestion_footer(&mut out, r.staged.mode, p.actions.iter());
    }
    staged_footer(
        &mut out,
        r.staged.mode,
        r.staged.result.as_ref(),
        p.changes(),
        "Nothing to do.",
        |out, res| {
            let _ = writeln!(
                out,
                "✓ added {} ROM(s), {}{}",
                res.added,
                bytes(res.bytes),
                if res.duplicates.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; {} already in the library (same bytes), dropped",
                        res.duplicates.len()
                    )
                }
            );
            for f in &res.failed {
                let _ = writeln!(out, "✗ {}: {}", f.source.display(), f.error.message);
            }
            covers_summary(out, r.covers.as_ref(), r.staged.mode);
        },
    );
    out
}

pub fn library_list(r: &ListReport) -> String {
    let mut out = String::new();
    if r.games.is_empty() {
        let _ = writeln!(out, "No games match.");
        return out;
    }
    let rows: Vec<[String; 4]> = r
        .games
        .iter()
        .flat_map(|g| {
            g.roms.iter().map(move |rom| {
                [
                    g.platform.clone(),
                    g.name.clone(),
                    if rom.verified {
                        "✓".into()
                    } else {
                        "?".into()
                    },
                    bytes(rom.size),
                ]
            })
        })
        .collect();
    table(&mut out, ["PLATFORM", "GAME", "DAT", "SIZE"], &rows);
    let _ = writeln!(
        out,
        "\n{} game(s), {} ROM(s), {}",
        r.games.len(),
        r.roms,
        bytes(r.bytes)
    );
    out
}

fn rename_kind(k: RenameKind) -> &'static str {
    match k {
        RenameKind::Rom => "ROM",
        RenameKind::Save => "save",
        RenameKind::SaveState => "state",
    }
}

pub fn library_rename(r: &RenameReport) -> String {
    let mut out = String::new();
    let p = &r.plan;
    let _ = writeln!(out, "[{}] {}  →  {}", p.platform, p.old_name, p.new_name);
    if p.new_game_name != p.old_game_name {
        let _ = writeln!(out, "  game   {}  →  {}", p.old_game_name, p.new_game_name);
    }
    match &p.library_file {
        Some(f) => {
            let _ = writeln!(
                out,
                "  library  {}  →  {}",
                f.from.display(),
                f.to.display()
            );
        }
        None => {
            let _ = writeln!(out, "  library  (referenced file; index only)");
        }
    }
    for f in &p.library_saves {
        let _ = writeln!(
            out,
            "  library  {} {}  →  {}",
            rename_kind(f.kind),
            f.from.display(),
            f.to.display()
        );
    }
    match &p.card {
        Some(card) => {
            for f in &card.renames {
                let _ = writeln!(
                    out,
                    "  card     {} {}  →  {}",
                    rename_kind(f.kind),
                    f.from.display(),
                    f.to.display()
                );
            }
        }
        None => {
            let _ = writeln!(
                out,
                "  card     not mounted or no copy there; remembered, the next device sync renames it"
            );
        }
    }
    staged_footer(
        &mut out,
        r.mode,
        r.result.as_ref(),
        p.changes().max(1),
        "",
        |out, res| {
            let _ = writeln!(
                out,
                "✓ renamed ({} library, {} card file(s))",
                res.library_renamed, res.card_renamed
            );
            for f in &res.failed {
                let _ = writeln!(out, "✗ {}: {}", f.path.display(), f.error.message);
            }
        },
    );
    out
}

pub fn library_remove(r: &RemoveReport) -> String {
    let mut out = String::new();
    let p = &r.plan;
    for item in &p.items {
        let _ = writeln!(
            out,
            "  - [{}] {}  {}{}{}",
            item.platform,
            item.name,
            bytes(item.size),
            if item.file.is_none() {
                "  (reference; file stays)"
            } else {
                ""
            },
            if item.last_of_game && item.game != item.name {
                format!("  [game {} goes too]", item.game)
            } else {
                String::new()
            }
        );
    }
    let _ = writeln!(
        out,
        "{} ROM(s), {}  →  {}/{}",
        p.items.len(),
        bytes(p.bytes),
        p.trash.display(),
        if r.mode == Mode::Interactive {
            "  (cards are untouched; a device sync with deletes removes them there)"
        } else {
            "  (cards are untouched; `device sync --delete` removes them there)"
        }
    );
    staged_footer(
        &mut out,
        r.mode,
        r.result.as_ref(),
        p.changes(),
        "Nothing to do.",
        |out, res| {
            let _ = writeln!(
                out,
                "✓ removed {} ROM(s), {} game(s){}",
                res.removed,
                res.games_removed,
                res.trash
                    .as_ref()
                    .map(|t| format!("; files in {}", t.display()))
                    .unwrap_or_default()
            );
            for f in &res.failed {
                let _ = writeln!(out, "✗ {}: {}", f.name, f.error.message);
            }
        },
    );
    out
}

pub fn library_standardize(r: &StandardizeReport) -> String {
    let mut out = String::new();
    let renames = &r.plan.renames;
    if renames.is_empty() {
        let _ = writeln!(
            out,
            "Every unverified game already has a standard name, or no standard name matches."
        );
        return out;
    }
    for p in renames {
        let _ = writeln!(out, "  [{}] {}  →  {}", p.platform, p.old_name, p.new_name);
        if let Some(card) = &p.card {
            let _ = writeln!(
                out,
                "           + {} file(s) on the card",
                card.renames.len()
            );
        }
    }
    if r.result.is_none() {
        let _ = writeln!(
            out,
            "{} game(s) would be renamed (library file, game, card copy and saves).",
            renames.len()
        );
    }
    staged_footer(
        &mut out,
        r.mode,
        r.result.as_ref(),
        renames.len(),
        "",
        |out, res| {
            let ok = res.iter().filter(|x| x.failed.is_empty()).count();
            let _ = writeln!(out, "✓ renamed {ok} of {} game(s)", res.len());
            for x in res.iter().flat_map(|x| x.failed.iter()) {
                let _ = writeln!(out, "✗ {}: {}", x.path.display(), x.error.message);
            }
        },
    );
    out
}

pub fn covers_fetch_text(r: &FetchReport) -> String {
    let mut out = String::new();
    plan_lines(
        &mut out,
        r.items.iter().map(|item| {
            let glyph = match item.status {
                CoverStatus::Ok => "✓",
                CoverStatus::None => "·",
                CoverStatus::Error => "✗",
            };
            format!(
                "  {glyph} {} / {}{}",
                item.platform,
                item.game,
                item.message
                    .as_deref()
                    .map(|m| format!("  ({m})"))
                    .unwrap_or_default()
            )
        }),
    );
    let s = &r.summary;
    let _ = writeln!(
        out,
        "{} fetched, {} already had covers, {} not on libretro, {} error(s)",
        s.fetched, s.already, s.not_found, s.errors
    );
    if !r.unsupported_platforms.is_empty() {
        let _ = writeln!(
            out,
            "No libretro mapping for: {} (pass --libretro-system \"<libretro system name>\" with --platform)",
            r.unsupported_platforms.join(", ")
        );
    }
    out
}

pub fn covers_status_text(r: &CoversStatusReport) -> String {
    let mut out = String::new();
    if r.platforms.is_empty() {
        let _ = writeln!(out, "No games in the library.");
        return out;
    }
    let rows: Vec<[String; 4]> = r
        .platforms
        .iter()
        .map(|s| {
            [
                s.platform.clone(),
                format!("{}/{}", s.with_cover, s.games),
                s.not_found.to_string(),
                if s.supported {
                    "yes".into()
                } else {
                    "no (--libretro-system)".into()
                },
            ]
        })
        .collect();
    table(
        &mut out,
        ["PLATFORM", "COVERS", "NOT FOUND", "MAPPED"],
        &rows,
    );
    out
}
