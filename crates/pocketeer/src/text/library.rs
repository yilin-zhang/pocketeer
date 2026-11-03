//! Text for the library: its contents, imports, renames, removals, DAT
//! checks and covers.

use std::{collections::BTreeMap, fmt::Write};

use pocketeer_core::{
    library::{
        Status as LibraryStatus,
        covers::{
            CoverStatus, FetchReport,
            import::{CoverImportPlan, CoverImportResult},
        },
        identify,
        import::{ImportAction, ImportPlan, ImportResult, ImportSkip},
        remove::{RemovePlan, RemoveResult},
        rename::{RenamePlan, RenameResult},
    },
    plan::Plan,
    transfer::from_card::CardImportPlan,
};

use super::{
    Text, bytes, failure_lines, import_actions, plan_lines, skipped_summary, table, warning_lines,
};
use crate::ops::library::{CoversStatusReport, InitReport, ListReport};

impl Text for identify::Report {
    fn text(&self) -> String {
        let r = self;
        let mut out = String::new();
        for error in &r.errors {
            let _ = writeln!(out, "✗ {error}");
        }
        let mut counts: Vec<String> = identify_news(r)
            .into_iter()
            .map(|(n, what)| format!("{n} {what}"))
            .collect();
        for (n, what) in [
            (r.unsupported, "on a platform without a DAT"),
            (r.unchanged, "unchanged"),
        ] {
            if n > 0 {
                counts.push(format!("{n} {what}"));
            }
        }
        let total = r.matched + r.no_match + r.ambiguous + r.unsupported + r.unchanged;
        let _ = match (total, r.errors.len()) {
            (0, 0) => writeln!(out, "No ROMs to check."),
            (_, 0) => writeln!(out, "✓ checked {total} ROM(s): {}", counts.join(", ")),
            // The errors above already say everything.
            (0, _) => Ok(()),
            (_, e) => writeln!(
                out,
                "! checked {total} ROM(s): {}, {e} error(s)",
                counts.join(", ")
            ),
        };
        out
    }
}

/// What a DAT check found that is worth mentioning: new results only.
pub fn identify_news(r: &identify::Report) -> Vec<(usize, &'static str)> {
    [
        (r.matched, "identified"),
        (r.no_match, "not in any DAT"),
        (r.ambiguous, "matching several DAT entries"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .collect()
}

impl Text for InitReport {
    fn text(&self) -> String {
        format!("Created library at {}\n", self.root.display())
    }
}

impl Text for LibraryStatus {
    fn text(&self) -> String {
        let s = self;
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
}

/// What each platform gets, and what is left out.
impl Text for ImportPlan {
    fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{}  →  library (copies; the source stays)",
            self.dir.display()
        );
        let mut skipped = BTreeMap::new();
        for platform in self.platforms() {
            let actions = self.actions().iter().filter(|a| a.platform() == platform);
            // Platforms with nothing new show up only in the skipped count.
            if actions
                .clone()
                .any(|a| matches!(a, ImportAction::Add { .. }))
            {
                let _ = writeln!(out, "[{platform}]");
            }
            for (reason, n) in import_actions(&mut out, actions) {
                *skipped.entry(reason).or_default() += n;
            }
        }
        let s = self.summary();
        if s.add + s.skip > 0 {
            let _ = writeln!(
                out,
                "{} to add ({} verified, {} unverified, {}){}",
                s.add,
                s.verified,
                s.unverified,
                bytes(s.bytes),
                skipped_summary(&skipped)
            );
        }
        if !self.unknown.is_empty() {
            let _ = writeln!(out, "Left out, type unknown ({}):", self.unknown.len());
            plan_lines(&mut out, self.unknown.iter().map(|u| format!("  {u}")));
        }
        if self.changes() == 0 {
            let _ = writeln!(out, "Nothing to import.");
        }
        out
    }
}

/// A folder import, plus the card paths asked for that aren't there.
impl Text for CardImportPlan {
    fn text(&self) -> String {
        let mut out = self.plan.text();
        for n in &self.not_found {
            let _ = writeln!(out, "✗ {}: not an unmatched ROM on this card", n.display());
        }
        out
    }
}

impl Text for ImportResult {
    fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "✓ added {} ROM(s), {}{}",
            self.added,
            bytes(self.bytes),
            if self.duplicates.is_empty() {
                String::new()
            } else {
                format!(
                    "; {} already in the library (same bytes), dropped",
                    self.duplicates.len()
                )
            }
        );
        failure_lines(&mut out, &self.failed);
        warning_lines(&mut out, &self.warnings);
        out
    }
}

impl Text for ListReport {
    fn text(&self) -> String {
        let r = self;
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
                        rom.dat.status.label().into(),
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
}

impl Text for RenamePlan {
    fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "[{}] {}  →  {}",
            self.rom.platform, self.rom.name, self.new_name
        );
        if self.new_game_name != self.rom.game {
            let _ = writeln!(out, "  game   {}  →  {}", self.rom.game, self.new_game_name);
        }
        let f = &self.library_file;
        let _ = writeln!(
            out,
            "  library  {}  →  {}",
            f.from.display(),
            f.to.display()
        );
        out
    }
}

impl Text for RenameResult {
    fn text(&self) -> String {
        format!("✓ renamed {} library file(s)\n", self.library_renamed)
    }
}

impl Text for RemovePlan {
    fn text(&self) -> String {
        let mut out = String::new();
        for item in &self.items {
            let _ = writeln!(
                out,
                "  - [{}] {}  {}{}",
                item.rom.platform,
                item.rom.name,
                bytes(item.size),
                if item.last_of_game && item.rom.game != item.rom.name {
                    format!("  [game {} goes too]", item.rom.game)
                } else {
                    String::new()
                }
            );
        }
        let _ = writeln!(
            out,
            "{} ROM(s), {}  →  {}/  (card copies are managed separately)",
            self.items.len(),
            bytes(self.bytes),
            self.trash.display()
        );
        if self.items.is_empty() {
            let _ = writeln!(out, "Nothing to do.");
        }
        out
    }
}

impl Text for RemoveResult {
    fn text(&self) -> String {
        let mut out = format!(
            "✓ removed {} ROM(s), {} game(s){}\n",
            self.removed,
            self.games_removed,
            self.trash
                .as_ref()
                .map(|t| format!("; files in {}", t.display()))
                .unwrap_or_default()
        );
        failure_lines(&mut out, &self.failed);
        out
    }
}

impl Text for FetchReport {
    fn text(&self) -> String {
        let r = self;
        let mut out = String::new();
        plan_lines(
            &mut out,
            r.items.iter().map(|item| {
                let glyph = match item.status {
                    CoverStatus::Ok => "✓",
                    CoverStatus::NotFound => "·",
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
        let counts: Vec<String> = [
            (s.fetched, "downloaded"),
            (s.already, "already there"),
            (s.not_found, "not on libretro"),
            (s.errors, "failed"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
        let _ = if counts.is_empty() {
            writeln!(out, "No games to fetch covers for.")
        } else {
            writeln!(
                out,
                "{} covers: {}",
                if s.errors > 0 { "!" } else { "✓" },
                counts.join(", ")
            )
        };
        if !r.unsupported_platforms.is_empty() {
            let _ = writeln!(
                out,
                "No libretro mapping for {}.",
                r.unsupported_platforms.join(", ")
            );
        }
        out
    }
}

impl Text for CoversStatusReport {
    fn text(&self) -> String {
        let r = self;
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
                    format!("{}/{}", s.covers, s.games),
                    s.covers_not_found.to_string(),
                    if s.covers_mapped { "yes" } else { "no" }.into(),
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
}

impl Text for CoverImportPlan {
    fn text(&self) -> String {
        let mut out = format!(
            "{} / {}\nCopy {} ({} × {}, {})\n  → {}\n",
            self.game.platform,
            self.game.name,
            self.source.display(),
            self.width,
            self.height,
            bytes(self.bytes as u64),
            self.destination.display()
        );
        if self.replaces_cover() {
            out.push_str("Replaces the current cover after the new copy is saved.\n");
        }
        out.push_str("The source image is kept. Automatic downloads will keep this cover.\n");
        out
    }
}

impl Text for CoverImportResult {
    fn text(&self) -> String {
        let mut out = format!("✓ Cover imported: {}\n", self.path.display());
        warning_lines(&mut out, &self.warnings);
        out
    }
}

/// A folder import in a few lines, for the TUI's dialog: per platform how
/// many files are new (and their names), what is already there, and the
/// files left out because their type isn't known.
pub fn import_brief(plan: &ImportPlan) -> Vec<String> {
    const NAMES: usize = 6;
    let mut lines = Vec::new();
    for platform in plan.platforms() {
        let mut new = Vec::new();
        let (mut there, mut other) = (0, 0);
        for a in plan.actions().iter().filter(|a| a.platform() == platform) {
            match a {
                ImportAction::Add { rom, .. } => new.push(rom.as_str()),
                ImportAction::Skip {
                    reason: ImportSkip::AlreadyInLibrary { .. },
                    ..
                } => there += 1,
                ImportAction::Skip { .. } => other += 1,
            }
        }
        let mut counts = vec![format!("{} new", new.len())];
        if there > 0 {
            counts.push(format!("{there} already in the library"));
        }
        if other > 0 {
            counts.push(format!("{other} skipped"));
        }
        lines.push(format!(
            "{:<8}{}",
            platform.to_uppercase(),
            counts.join(", ")
        ));
        for name in new.iter().take(NAMES) {
            lines.push(format!("        {name}"));
        }
        if new.len() > NAMES {
            lines.push(format!("        … and {} more", new.len() - NAMES));
        }
    }
    let unknown = &plan.unknown;
    if !unknown.is_empty() {
        let names: Vec<&str> = unknown
            .iter()
            .take(3)
            .map(|u| u.rsplit(['/', ':']).next().unwrap_or(u))
            .collect();
        lines.push(format!(
            "Left out, type unknown: {}{}",
            names.join(", "),
            if unknown.len() > 3 {
                format!(" and {} more", unknown.len() - 3)
            } else {
                String::new()
            }
        ));
    }
    lines
}
