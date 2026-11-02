//! Text for `ops::library`.

use std::fmt::Write;

use pocketeer_core::{
    library::{
        Status as LibraryStatus,
        covers::{CoverStatus, FetchReport},
        identify,
        import::{ImportAction, ImportPlan, ImportResult, ImportSkip},
    },
    plan::Plan,
};

use super::{
    bytes, cover_news, covers_summary, import_actions, plan_lines, skipped_summary, staged_footer,
    suggestion_footer, table, unmapped_hint, warning_lines,
};
use crate::ops::{
    Mode,
    library::{
        CoverImportReport, CoversStatusReport, ImportReport, InitReport, ListReport,
        MetadataReport, RemoveReport, RenameReport, SortedImportReport, StandardizeReport,
    },
};

pub fn identify_text(r: &identify::Report) -> String {
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

/// The TUI's background check, for its log: one summary line, which is
/// what the status bar ends up showing. It names the first problem; when
/// there are more, all of them are listed above it.
pub fn metadata_status(r: &MetadataReport) -> String {
    let mut out = String::new();
    if r.problems().nth(1).is_some() {
        for problem in r.problems() {
            let _ = writeln!(out, "! {problem}");
        }
    }
    if let Some(hint) = r
        .covers
        .as_ref()
        .and_then(|c| unmapped_hint(c, Mode::Interactive))
    {
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
    let _ = writeln!(out, "{}  →  library/{} (copy)", p.dir.display(), p.platform);
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
            warning_lines(out, &r.warnings);
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

pub fn library_rename(r: &RenameReport) -> String {
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
        r.result.as_ref(),
        p.changes(),
        "",
        |out, res| {
            let _ = writeln!(out, "✓ renamed {} library file(s)", res.library_renamed);
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
    }
    if r.result.is_none() {
        let _ = writeln!(
            out,
            "{} game(s) would be renamed (library file and game).",
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
            let ok = res.iter().filter(|x| x.is_ok()).count();
            let _ = writeln!(out, "✓ renamed {ok} of {} game(s)", res.len());
            for (plan, result) in renames.iter().zip(res.iter()) {
                if let Err(error) = result {
                    let _ = writeln!(
                        out,
                        "✗ [{}] {}: {}",
                        plan.platform, plan.old_name, error.message
                    );
                }
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
    if let Some(hint) = unmapped_hint(r, Mode::Applied) {
        let _ = writeln!(out, "{hint}");
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

pub fn cover_import(staged: &CoverImportReport) -> String {
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

/// A folder import in a few lines, for the TUI's dialog: per platform how
/// many files are new (and their names), what is already there, and the
/// files left out because their type isn't known.
pub fn import_brief(parts: &[ImportPlan], unknown: &[String]) -> Vec<String> {
    const NAMES: usize = 6;
    let mut lines = Vec::new();
    for plan in parts {
        let mut new = Vec::new();
        let (mut there, mut other) = (0, 0);
        for a in &plan.actions {
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
            plan.platform.to_uppercase(),
            counts.join(", ")
        ));
        for name in new.iter().take(NAMES) {
            lines.push(format!("        {name}"));
        }
        if new.len() > NAMES {
            lines.push(format!("        … and {} more", new.len() - NAMES));
        }
    }
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

/// One platform of a finished TUI import, in a line.
pub fn import_done(platform: &str, r: &ImportResult) -> String {
    let mut line = format!("✓ {}: added {}", platform.to_uppercase(), r.added);
    if !r.duplicates.is_empty() {
        let _ = write!(line, ", {} already there", r.duplicates.len());
    }
    for f in &r.failed {
        let _ = write!(line, "\n✗ {}: {}", f.source.display(), f.error.message);
    }
    line
}

/// `library import` without `--platform`: each platform's report in turn.
pub fn library_import_sorted(r: &SortedImportReport) -> String {
    let mut out = String::new();
    if r.parts.is_empty() && r.unknown.is_empty() {
        let _ = writeln!(out, "No ROM files found.");
    }
    for part in &r.parts {
        out.push_str(&library_import(part));
        out.push('\n');
    }
    if !r.unknown.is_empty() {
        let _ = writeln!(
            out,
            "Left out, type unknown ({}); import them with --platform:",
            r.unknown.len()
        );
        for u in &r.unknown {
            let _ = writeln!(out, "  {u}");
        }
    }
    out
}
