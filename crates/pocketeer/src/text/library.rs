//! Text for `ops::library`.

use std::{collections::BTreeMap, fmt::Write};

use pocketeer_core::{
    library::{
        Status as LibraryStatus,
        covers::{CoverStatus, FetchReport},
        identify,
        import::{ImportAction, ImportPlan, ImportSkip},
    },
    plan::Plan,
};

use super::{
    Reader, Text, batch_report, bytes, cover_news, covers_summary, failure_lines, import_actions,
    plan_lines, skipped_summary, staged_footer, suggestion_footer, table, unmapped_hint,
    warning_lines,
};
use crate::ops::library::{
    CoverImportReport, CoversStatusReport, Imported, ImportsRoms, InitReport, ListReport,
    MetadataReport, RemoveReport, RenameReport, StandardizeReport,
};

impl Text for identify::Report {
    fn text(&self, _: Reader) -> String {
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
fn identify_news(r: &identify::Report) -> Vec<(usize, &'static str)> {
    [
        (r.matched, "identified"),
        (r.no_match, "not in any DAT"),
        (r.ambiguous, "matching several DAT entries"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .collect()
}

impl Text for MetadataReport {
    /// The TUI's background check, for its log: one summary line, which is
    /// what the status bar ends up showing. It names the first problem; when
    /// there are more, all of them are listed above it.
    fn text(&self, reader: Reader) -> String {
        let r = self;
        let mut out = String::new();
        if r.problems().nth(1).is_some() {
            for problem in r.problems() {
                let _ = writeln!(out, "! {problem}");
            }
        }
        if let Some(hint) = r.covers.as_ref().and_then(|c| unmapped_hint(c, reader)) {
            let _ = writeln!(out, "{hint}");
        }
        let mut news = Vec::new();
        if let Some(id) = &r.identify {
            news.extend(
                identify_news(id)
                    .into_iter()
                    .map(|(n, what)| format!("{n} ROM(s) {what}")),
            );
        }
        if let Some(c) = &r.covers {
            news.extend(
                cover_news(c)
                    .into_iter()
                    .map(|(n, what)| format!("{n} cover(s) {what}")),
            );
        }
        let checked = if r.covers.is_some() {
            "DATs and covers checked"
        } else {
            "DATs checked"
        };
        // The status bar shows only this line, so it names the first problem.
        let mut problems = r.problems();
        let mark = match problems.next() {
            Some(first) => {
                let more = problems.count();
                news.push(if more > 0 {
                    format!("{first} (and {more} more problem(s))")
                } else {
                    first.to_string()
                });
                "!"
            }
            None => "✓",
        };
        let _ = if news.is_empty() {
            writeln!(out, "{mark} {checked}, nothing new")
        } else {
            writeln!(out, "{mark} {checked}: {}", news.join(", "))
        };
        out
    }
}

impl Text for InitReport {
    fn text(&self, _: Reader) -> String {
        let r = self;
        format!(
            "Created library at {}\nNext: pocketeer library import <folder> --platform <id> [--dat <file>]\n",
            r.root.display()
        )
    }
}

impl Text for LibraryStatus {
    fn text(&self, _: Reader) -> String {
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

impl<P: ImportsRoms> Text for Imported<P> {
    /// `library import` and `card import`: what each platform gets, what is left
    /// out, then what applying did.
    fn text(&self, reader: Reader) -> String {
        let r = self;
        let mut out = String::new();
        let p = r.staged.plan.import();
        let not_found = r.staged.plan.not_found();
        let _ = writeln!(
            out,
            "{}  →  library (copies; the source stays)",
            p.dir.display()
        );
        let mut skipped = BTreeMap::new();
        for platform in p.platforms() {
            let actions = p.actions.iter().filter(|a| a.platform() == platform);
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
        let s = &p.summary;
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
        for n in not_found {
            let _ = writeln!(out, "✗ {}: not an unmatched ROM on this card", n.display());
        }
        if !p.unknown.is_empty() {
            let _ = writeln!(
                out,
                "Left out, type unknown ({}){}:",
                p.unknown.len(),
                if reader == Reader::Tui {
                    ""
                } else {
                    "; import them with --platform"
                }
            );
            plan_lines(&mut out, p.unknown.iter().map(|u| format!("  {u}")));
        }
        if r.staged.result.is_none() && p.changes() > 0 {
            suggestion_footer(&mut out, reader, p.actions.iter());
        }
        // Nothing is applied while a name is missing, so no footer then either.
        let changes = if not_found.is_empty() { p.changes() } else { 0 };
        staged_footer(
            &mut out,
            r.staged.mode,
            reader,
            r.staged.result.as_ref(),
            changes,
            "Nothing to import.",
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
                failure_lines(out, &res.failed);
                warning_lines(out, &res.warnings);
                covers_summary(out, r.covers.as_ref(), reader);
                warning_lines(out, &r.warnings);
            },
        );
        out
    }
}

impl Text for ListReport {
    fn text(&self, _: Reader) -> String {
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

impl Text for RenameReport {
    fn text(&self, reader: Reader) -> String {
        let r = self;
        let mut out = String::new();
        let p = &r.plan;
        let _ = writeln!(out, "[{}] {}  →  {}", p.platform, p.old_name, p.new_name);
        if p.new_game_name != p.old_game_name {
            let _ = writeln!(out, "  game   {}  →  {}", p.old_game_name, p.new_game_name);
        }
        let f = &p.library_file;
        let _ = writeln!(
            out,
            "  library  {}  →  {}",
            f.from.display(),
            f.to.display()
        );
        staged_footer(
            &mut out,
            r.mode,
            reader,
            r.result.as_ref(),
            p.changes(),
            "",
            |out, res| {
                let _ = writeln!(out, "✓ renamed {} library file(s)", res.library_renamed);
            },
        );
        out
    }
}

impl Text for RemoveReport {
    fn text(&self, reader: Reader) -> String {
        let r = self;
        let mut out = String::new();
        let p = &r.plan;
        for item in &p.items {
            let _ = writeln!(
                out,
                "  - [{}] {}  {}{}",
                item.platform,
                item.name,
                bytes(item.size),
                if item.last_of_game && item.game != item.name {
                    format!("  [game {} goes too]", item.game)
                } else {
                    String::new()
                }
            );
        }
        let _ = writeln!(
            out,
            "{} ROM(s), {}  →  {}/  (card copies are managed separately)",
            p.items.len(),
            bytes(p.bytes),
            p.trash.display()
        );
        staged_footer(
            &mut out,
            r.mode,
            reader,
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
                failure_lines(out, &res.failed);
            },
        );
        out
    }
}

impl Text for StandardizeReport {
    fn text(&self, reader: Reader) -> String {
        let r = self;
        if r.items.is_empty() {
            return "Every unverified game already has a standard name, or no standard name matches.\n"
                .into();
        }
        batch_report(
            r,
            reader,
            |out, p, _| {
                let _ = writeln!(out, "  [{}] {}  →  {}", p.platform, p.old_name, p.new_name);
            },
            |_| "renamed".into(),
        )
    }
}

impl Text for FetchReport {
    fn text(&self, reader: Reader) -> String {
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
        if let Some(hint) = unmapped_hint(r, reader) {
            let _ = writeln!(out, "{hint}");
        }
        out
    }
}

impl Text for CoversStatusReport {
    fn text(&self, _: Reader) -> String {
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
}

impl Text for CoverImportReport {
    fn text(&self, reader: Reader) -> String {
        let staged = self;
        let plan = &staged.plan;
        let mut out = format!(
            "{} / {}\nCopy {} ({} × {}, {})\n  → {}\n",
            plan.game.platform,
            plan.game.name,
            plan.source.display(),
            plan.width,
            plan.height,
            bytes(plan.bytes as u64),
            plan.destination.display()
        );
        if plan.previous.as_ref().is_some_and(|p| p.rel_path.is_some()) {
            out.push_str("Replaces the current cover after the new copy is saved.\n");
        }
        out.push_str("The source image is kept. Automatic downloads will keep this cover.\n");
        staged_footer(
            &mut out,
            staged.mode,
            reader,
            staged.result.as_ref(),
            1,
            "",
            |out, result| {
                let _ = writeln!(out, "✓ Cover imported: {}", result.path.display());
                for warning in &result.warnings {
                    let _ = writeln!(out, "! {warning}");
                }
            },
        );
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
        for a in plan.actions.iter().filter(|a| a.platform() == platform) {
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
