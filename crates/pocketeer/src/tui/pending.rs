//! Plans the TUI shows before applying them. A flow plans on the worker
//! (a [`Planner`]) and gets back the dialog to open ([`Preview`]); a plan
//! confirmed there is applied on the worker too. Each kind of plan says how
//! it is asked about and applied by implementing [`Pending`] here. Most ask
//! over the plan's own text and report the result's (see `text`); the
//! dialog's buttons say what happens next.

use std::{collections::BTreeMap, path::PathBuf};

use pocketeer_core::{
    Result,
    card::{
        bios::PlacePlan,
        clean::CleanPlan,
        install::{Prepared, UninstallPlan},
        roms::DeletePlan,
    },
    library::{
        covers::import::CoverImportPlan, import::ImportPlan,
        remove::RemovePlan as LibraryRemovePlan, rename::RenamePlan,
    },
    names,
    plan::{self, Applied, Plan, Progress},
    transfer::{
        from_card::CardImportPlan,
        rename::CardRenamePlan,
        saves::{PullReason, SaveAction, SavesPlan, TrashPlan, TrashTarget},
        to_card::CopyPlan,
    },
};

use super::worker::{Msg, applied, failed};
use crate::{
    ops::{Batch, context::Context},
    text::{self, Label, Text},
};

/// Planning work for the worker: the plan, and the dialog it opens.
pub type Planner = Box<dyn FnOnce(&Context, Progress<'_>) -> Result<Preview> + Send>;

/// A plan the TUI can ask about and then apply.
pub trait Pending: Send {
    /// The dialog it opens before it is applied.
    fn preview(self: Box<Self>) -> Preview;
    /// Applies it and says what happened.
    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg;
    /// Imports add games one by one; the Games tab refreshes after each.
    fn adds_games(&self) -> bool {
        false
    }
}

/// The dialog `pending` opens: what planners return.
pub fn preview(pending: impl Pending + 'static) -> Preview {
    Box::new(pending).preview()
}

/// The dialog a plan opens before it is applied.
pub enum Preview {
    /// Nothing to do: one line for the status bar.
    Notice(String),
    /// Nothing to do, but worth reading.
    Info { title: String, lines: Vec<String> },
    /// Apply / Cancel.
    Confirm {
        title: String,
        lines: Vec<String>,
        pending: Box<dyn Pending>,
    },
    /// A save changed on both sides: which copy wins (`index` into the plan).
    SaveConflict { backup: Box<SaveSync>, index: usize },
    /// Standard-name renames to tick.
    PickRenames(Vec<RenamePlan>),
    /// An import whose files match standard names: which names to take.
    PickImportNames(Box<Import>),
    /// An import whose files don't say their platform.
    AskPlatform { source: PathBuf, files: usize },
}

pub(super) fn text_lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

fn confirm(
    title: impl Into<String>,
    lines: Vec<String>,
    pending: impl Pending + 'static,
) -> Preview {
    Preview::Confirm {
        title: title.into(),
        lines,
        pending: Box::new(pending),
    }
}

fn info(title: &str, lines: Vec<String>) -> Preview {
    Preview::Info {
        title: title.into(),
        lines,
    }
}

/// Asks `title` over the plan's own text.
fn ask<P: Plan + Text + Pending + 'static>(title: impl Into<String>, plan: P) -> Preview {
    let lines = text_lines(&plan.text());
    confirm(title, lines, plan)
}

/// Applies `plan` and says what applying did.
fn apply_plan<P: Plan>(mut plan: P, ctx: &Context, progress: Progress<'_>) -> Msg
where
    P::Output: Text,
{
    match plan::apply(&mut ctx.resources(), &mut plan, progress) {
        Ok(result) => applied(result.text(), result.failed()),
        Err(e) => failed(e),
    }
}

// ---------------------------------------------------------------------------
// Plans asked about over their own text.

impl Pending for CoverImportPlan {
    fn preview(self: Box<Self>) -> Preview {
        let title = match self.replaces_cover() {
            true => "Replace this game's cover?",
            false => "Import this game's cover?",
        };
        ask(title, *self)
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_plan(*self, ctx, progress)
    }
}

impl Pending for CardRenamePlan {
    fn preview(self: Box<Self>) -> Preview {
        ask("Rename this AP ROM and its saves?", *self)
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_plan(*self, ctx, progress)
    }
}

impl Pending for CleanPlan {
    fn preview(self: Box<Self>) -> Preview {
        if self.entries.is_empty() {
            return Preview::Notice("No macOS junk on the card.".into());
        }
        let title = format!(
            "Delete {} junk item(s), {}?",
            self.entries.len(),
            text::bytes(self.bytes)
        );
        ask(title, *self)
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_plan(*self, ctx, progress)
    }
}

impl Pending for CopyPlan {
    fn preview(self: Box<Self>) -> Preview {
        match self.changes() {
            0 => info("Already on this card", text_lines(&self.text())),
            n => ask(format!("Copy {n} ROM(s) to this card?"), *self),
        }
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_plan(*self, ctx, progress)
    }
}

impl Pending for RenamePlan {
    fn preview(self: Box<Self>) -> Preview {
        ask(format!("Rename to {}?", self.new_name), *self)
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_plan(*self, ctx, progress)
    }
}

impl Pending for PlacePlan {
    /// Zipped root files are named; pocketeer doesn't unpack them.
    fn preview(self: Box<Self>) -> Preview {
        if !self.actions.is_empty() {
            return ask(
                format!("Move {} file(s) into place?", self.actions.len()),
                *self,
            );
        }
        if self.zipped.is_empty() {
            return Preview::Notice("No file at the SD card root matches a missing file.".into());
        }
        let lines = self
            .zipped
            .iter()
            .map(|z| {
                format!(
                    "{} is inside a zip at the root; unzip it first.",
                    z.path.display()
                )
            })
            .collect();
        info("Nothing to place yet", lines)
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_plan(*self, ctx, progress)
    }
}

// ---------------------------------------------------------------------------
// Plans with a dialog of their own.

impl Pending for LibraryRemovePlan {
    fn preview(self: Box<Self>) -> Preview {
        let games: std::collections::BTreeSet<&str> =
            self.items.iter().map(|i| i.rom.game.as_str()).collect();
        let title = match (games.len(), games.iter().next()) {
            (1, Some(game)) => format!("Remove {game}?"),
            _ => format!("Remove {} games?", games.len()),
        };
        let mut lines: Vec<String> = if self.items.len() > 1 {
            self.items
                .iter()
                .map(|i| format!("{}  {}", i.rom.name, text::bytes(i.size)))
                .collect()
        } else {
            Vec::new()
        };
        lines.push("It goes to .trash/ in the library. Copies on the card stay.".into());
        confirm(title, lines, *self)
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_plan(*self, ctx, progress)
    }
}

impl Pending for TrashPlan {
    fn preview(self: Box<Self>) -> Preview {
        if self.files.is_empty() {
            return Preview::Notice("Nothing to throw away.".into());
        }
        let title = match self.target {
            TrashTarget::Card => format!("Delete {} file(s) from AP?", self.files.len()),
            _ => format!("Move {} file(s) to the trash?", self.files.len()),
        };
        let lines = trash_lines(&self);
        confirm(title, lines, *self)
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_plan(*self, ctx, progress)
    }
}

/// The files a trash plan throws away, by name, and where they go.
fn trash_lines(plan: &TrashPlan) -> Vec<String> {
    let name = |p: &std::path::Path| {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    let mut lines: Vec<String> = plan
        .files
        .iter()
        .map(|f| match plan.target {
            TrashTarget::Card => name(&f.path),
            _ if f.path.starts_with("saves/.history") => {
                format!("{}  older version, {}", name(&f.save), text::bytes(f.size))
            }
            _ => format!("{}  backup, {}", name(&f.save), text::bytes(f.size)),
        })
        .collect();
    lines.push(String::new());
    lines.push(match plan.target {
        TrashTarget::Card if plan.files.iter().any(|f| f.archive_first) => {
            "The library keeps a copy first.".into()
        }
        TrashTarget::Card => "The library already has a copy.".into(),
        _ => "They go to .trash/ in the library.".into(),
    });
    lines
}

/// Card path of a save → the name of the game it belongs to.
pub type SaveGames = BTreeMap<PathBuf, String>;

/// Saves to back up or restore, listed under the games they belong to.
pub struct SaveSync {
    pub plan: SavesPlan,
    pub games: SaveGames,
}

impl Pending for SaveSync {
    /// A conflict is settled first, one at a time.
    fn preview(self: Box<Self>) -> Preview {
        let conflict = self
            .plan
            .actions()
            .position(|a| matches!(a, SaveAction::Conflict { .. }));
        if let Some(index) = conflict {
            return Preview::SaveConflict {
                backup: self,
                index,
            };
        }
        let s = self.plan.summary();
        let title = match (s.pull, s.push) {
            (0, 0) => return Preview::Notice("Saves are already backed up.".into()),
            (n, 0) => format!("Back up {n} save(s)?"),
            (0, n) => format!("Put {n} file(s) back on AP?"),
            (a, b) => format!("Back up {a}, put {b} back on AP?"),
        };
        let lines = self.by_game();
        confirm(title, lines, *self)
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_plan(self.plan, ctx, progress)
    }
}

impl SaveSync {
    /// Saves under the game they belong to, each with what happens in a few
    /// words; the rest under "Other saves". Saves that stay as they are
    /// aren't listed.
    fn by_game(&self) -> Vec<String> {
        let mut groups: BTreeMap<(bool, String), Vec<String>> = BTreeMap::new();
        for a in self.plan.actions() {
            let what = match a {
                SaveAction::Pull {
                    reason: reason @ (PullReason::OnlyOnCard | PullReason::CardNewer),
                    ..
                } => format!("← back up, {}", reason.label()),
                SaveAction::Pull { .. } => "← back up".into(),
                SaveAction::Push { .. } => "→ put back on AP".into(),
                SaveAction::Conflict { .. } => "? changed on both sides, left as is".into(),
                SaveAction::Skip { .. } => continue,
            };
            let rel = a.path();
            let file = rel.file_name().unwrap_or_default().to_string_lossy();
            let (key, line) = match self.games.get(rel) {
                Some(game) if names::stem(&file) == game => {
                    ((false, game.clone()), format!("  {what}"))
                }
                Some(game) => ((false, game.clone()), format!("  {what}: {file}")),
                None => ((true, String::new()), format!("{what}: {file}")),
            };
            groups.entry(key).or_default().push(line);
        }
        let mut lines = Vec::new();
        for ((other, game), items) in groups {
            if !other {
                lines.push(game);
            } else if !lines.is_empty() {
                lines.push("Other saves".into());
            }
            lines.extend(items);
        }
        lines
    }
}

/// ROMs to copy into the library. A closed pair: the standard-names step
/// works on either.
pub enum Import {
    /// From a folder on this computer.
    Folder(ImportPlan),
    /// From the card.
    Card(CardImportPlan),
}

impl Import {
    pub fn plan(&self) -> &ImportPlan {
        match self {
            Import::Folder(p) => p,
            Import::Card(c) => &c.plan,
        }
    }

    pub fn plan_mut(&mut self) -> &mut ImportPlan {
        match self {
            Import::Folder(p) => p,
            Import::Card(c) => &mut c.plan,
        }
    }
}

impl Pending for Import {
    /// Standard names are offered first, if any match.
    fn preview(self: Box<Self>) -> Preview {
        let plan = self.plan();
        if plan.suggestions().next().is_some() {
            return Preview::PickImportNames(self);
        }
        let lines = text::library::import_brief(plan);
        let n = plan.changes();
        let title = match &*self {
            Import::Folder(_) => format!("Import {n} ROM(s)?"),
            Import::Card(_) => format!("Copy {n} ROM(s) into the library?"),
        };
        match n {
            0 => info("Nothing new to import", lines),
            _ => confirm(title, lines, *self),
        }
    }

    /// Without the follow-ups: the TUI identifies and fetches covers on its
    /// own afterwards.
    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        match *self {
            Import::Folder(plan) => apply_plan(plan, ctx, progress),
            Import::Card(plan) => apply_plan(plan, ctx, progress),
        }
    }

    fn adds_games(&self) -> bool {
        true
    }
}

impl Pending for Batch<Prepared> {
    fn preview(self: Box<Self>) -> Preview {
        let lines = text_lines(&self.text());
        match self.plans().filter(|p| p.changes() > 0).count() {
            0 => info("Nothing to do", lines),
            n => confirm(format!("Install {n} core(s)?"), lines, *self),
        }
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_batch(*self, ctx, progress)
    }
}

impl Pending for Batch<UninstallPlan> {
    fn preview(self: Box<Self>) -> Preview {
        let ids: Vec<&str> = self.items.iter().map(|i| i.target.as_str()).collect();
        let title = format!("Remove {}?", ids.join(", "));
        let lines = text_lines(&self.text());
        confirm(title, lines, *self)
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_batch(*self, ctx, progress)
    }
}

/// The standard names the user ticked.
impl Pending for Batch<RenamePlan> {
    fn preview(self: Box<Self>) -> Preview {
        let title = format!("Rename {} game(s) to standard names?", self.items.len());
        let lines = text_lines(&self.text());
        confirm(title, lines, *self)
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_batch(*self, ctx, progress)
    }
}

/// Applies every planned item of a previewed batch.
fn apply_batch<P: Plan + Text>(batch: Batch<P>, ctx: &Context, progress: Progress<'_>) -> Msg
where
    P::Output: Text,
{
    let batch = batch.apply(&mut ctx.resources(), progress);
    applied(batch.text(), batch.failed())
}

/// A ROM only the card has, deleted from it.
impl Pending for DeletePlan {
    fn preview(self: Box<Self>) -> Preview {
        let name = self.path.file_name().unwrap_or_default().to_string_lossy();
        let title = format!("Delete {name} from the card?");
        let lines = vec![
            format!("  - {}", self.path.display()),
            "The library has no copy; i copies it there first.".into(),
        ];
        confirm(title, lines, *self)
    }

    fn apply(mut self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        match plan::apply(&mut ctx.resources(), &mut *self, progress) {
            Ok(_) => applied(
                format!("✓ deleted {} from the card", self.path.display()),
                0,
            ),
            Err(e) => failed(e),
        }
    }
}
