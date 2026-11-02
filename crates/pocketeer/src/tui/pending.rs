//! Plans the TUI shows before applying them. A flow plans on the worker
//! (a [`Planner`]) and gets back the dialog to open ([`Preview`]); a plan
//! confirmed there is applied on the worker too. Each kind of plan says how
//! it is asked about and applied by implementing [`Pending`] here; the
//! text in both dialogs is the text the CLI prints (`Reader::Tui`).

use std::{collections::BTreeMap, path::PathBuf};

use pocketeer_core::{
    Result,
    card::{
        bios::PlacePlan,
        clean::CleanPlan,
        install::{Prepared, UninstallPlan},
        rename::CardRenamePlan,
        roms::DeletePlan,
    },
    library::{
        covers::import::CoverImportPlan, import::ImportPlan,
        remove::RemovePlan as LibraryRemovePlan, rename::RenamePlan,
    },
    plan::{Plan, Progress},
    transfer::{
        from_card::CardImportPlan,
        saves::{SaveAction, SavesPlan, TrashPlan, TrashTarget},
        to_card::CopyPlan,
    },
};

use super::worker::{Msg, applied, failed};
use crate::{
    ops::{
        Apply, Batch, Mode, Staged,
        context::Context,
        library::{Imported, ImportsRoms},
    },
    text::{self, Reader, Text},
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
fn confirm_plan<P: Apply + Pending + 'static>(title: String, plan: P) -> Preview
where
    Staged<P>: Text,
{
    let staged = Staged::preview(plan);
    let lines = text_lines(&staged.text(Reader::Tui));
    confirm(title, lines, staged.plan)
}

/// Applies `plan` and says what happened the way the CLI does.
fn apply_plan<P: Apply>(mut plan: P, ctx: &Context, progress: Progress<'_>) -> Msg
where
    Staged<P>: Text,
{
    match plan.apply(ctx, progress) {
        Ok(result) => {
            let staged = Staged {
                mode: Mode::Applied,
                plan,
                result: Some(result),
            };
            applied(staged.text(Reader::Tui), staged.failed())
        }
        Err(e) => failed(e),
    }
}

/// A plan asked about with its own text under a fixed kind of question;
/// [`Pending`] follows from it.
trait Ask: Apply + Send + Sized + 'static
where
    Staged<Self>: Text,
{
    /// The question, or `Err` with what to show when there is nothing to do.
    fn question(&self) -> std::result::Result<String, Nothing>;
}

/// What a plan with nothing to do shows instead of a question.
enum Nothing {
    /// A line in the status bar.
    Notice(&'static str),
    /// Its text, under this title.
    Info(&'static str),
}

impl<P: Ask> Pending for P
where
    Staged<P>: Text,
{
    fn preview(self: Box<Self>) -> Preview {
        match self.question() {
            Ok(title) => confirm_plan(title, *self),
            Err(Nothing::Notice(line)) => Preview::Notice(line.into()),
            Err(Nothing::Info(title)) => {
                info(title, text_lines(&Staged::preview(*self).text(Reader::Tui)))
            }
        }
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_plan(*self, ctx, progress)
    }
}

// ---------------------------------------------------------------------------
// Plans asked about with their own text.

impl Ask for CoverImportPlan {
    fn question(&self) -> std::result::Result<String, Nothing> {
        Ok(match self.replaces_cover() {
            true => "Replace this game's cover?".into(),
            false => "Import this game's cover?".into(),
        })
    }
}

impl Ask for CardRenamePlan {
    fn question(&self) -> std::result::Result<String, Nothing> {
        Ok("Rename this AP ROM and its saves?".into())
    }
}

impl Ask for CleanPlan {
    fn question(&self) -> std::result::Result<String, Nothing> {
        if self.entries.is_empty() {
            return Err(Nothing::Notice("No macOS junk on the card."));
        }
        Ok(format!(
            "Delete {} junk item(s), {}?",
            self.entries.len(),
            text::bytes(self.bytes)
        ))
    }
}

impl Ask for CopyPlan {
    fn question(&self) -> std::result::Result<String, Nothing> {
        match self.changes() {
            0 => Err(Nothing::Info("Already on this card")),
            n => Ok(format!("Copy {n} ROM(s) to this card?")),
        }
    }
}

impl Ask for RenamePlan {
    fn question(&self) -> std::result::Result<String, Nothing> {
        Ok(format!("Rename to {}?", self.new_name))
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

impl Pending for PlacePlan {
    /// Zipped root files are named; pocketeer doesn't unpack them.
    fn preview(self: Box<Self>) -> Preview {
        if !self.actions.is_empty() {
            return confirm_plan(
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

impl Pending for TrashPlan {
    fn preview(self: Box<Self>) -> Preview {
        if self.files.is_empty() {
            return Preview::Notice("Nothing to throw away.".into());
        }
        let title = match self.target {
            TrashTarget::Card => format!("Delete {} file(s) from AP?", self.files.len()),
            _ => format!("Move {} file(s) to the trash?", self.files.len()),
        };
        let lines = text::saves::trash_dialog(&self);
        confirm(title, lines, *self)
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_plan(*self, ctx, progress)
    }
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
        let lines = text::saves::saves_dialog(&self.plan, &self.games);
        confirm(title, lines, *self)
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        let SaveSync { mut plan, games } = *self;
        match plan.apply(ctx, progress) {
            Ok(result) => {
                let staged = Staged {
                    mode: Mode::Applied,
                    plan,
                    result: Some(result),
                };
                let text = text::saves::saves_sync_by_game(&staged, &games, Reader::Tui);
                applied(text, staged.failed())
            }
            Err(e) => failed(e),
        }
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
            Import::Card(c) => c.import(),
        }
    }

    pub fn plan_mut(&mut self) -> &mut ImportPlan {
        match self {
            Import::Folder(p) => p,
            Import::Card(c) => c.import_mut(),
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

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        match *self {
            Import::Folder(plan) => apply_import(plan, ctx, progress),
            Import::Card(plan) => apply_import(plan, ctx, progress),
        }
    }

    fn adds_games(&self) -> bool {
        true
    }
}

/// An import reads like `library import`, without the follow-ups (the TUI
/// identifies and fetches covers on its own afterwards).
fn apply_import<P: ImportsRoms>(mut plan: P, ctx: &Context, progress: Progress<'_>) -> Msg {
    match plan.apply(ctx, progress) {
        Ok(result) => {
            let imported = Imported {
                staged: Staged {
                    mode: Mode::Applied,
                    plan,
                    result: Some(result),
                },
                covers: None,
                warnings: Vec::new(),
            };
            applied(imported.text(Reader::Tui), imported.staged.failed())
        }
        Err(e) => failed(e),
    }
}

impl Pending for Batch<Prepared> {
    fn preview(self: Box<Self>) -> Preview {
        let lines = text_lines(&self.text(Reader::Tui));
        let n = self
            .items
            .iter()
            .filter_map(|i| i.plan())
            .filter(|p| p.changes() > 0)
            .count();
        match n {
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
        let lines = text_lines(&self.text(Reader::Tui));
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
        let lines = text_lines(&self.text(Reader::Tui));
        confirm(title, lines, *self)
    }

    fn apply(self: Box<Self>, ctx: &Context, progress: Progress<'_>) -> Msg {
        apply_batch(*self, ctx, progress)
    }
}

/// Applies every planned item of a previewed batch.
fn apply_batch<P: Apply + Send + 'static>(
    batch: Batch<P>,
    ctx: &Context,
    progress: Progress<'_>,
) -> Msg
where
    Batch<P>: Text,
{
    let plans = batch.into_plans().map(|(target, plan)| (target, Ok(plan)));
    let report = Batch::stage(ctx, Mode::Applied, plans, progress);
    applied(report.text(Reader::Tui), report.failed())
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
        match Apply::apply(&mut *self, ctx, progress) {
            Ok(_) => applied(
                format!("✓ deleted {} from the card", self.path.display()),
                0,
            ),
            Err(e) => failed(e),
        }
    }
}
