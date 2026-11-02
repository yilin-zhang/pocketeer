//! Text for `ops::library`.

use std::fmt::Write;

use pocketeer_core::{
    library::{
        Status as LibraryStatus,
        covers::{CoverStatus, FetchReport},
        identify,
    },
    plan::Plan,
};

use super::{
    bytes, covers_summary, import_actions, plan_lines, skipped_summary, staged_footer,
    suggestion_footer, table,
};
use crate::ops::library::{
    CoverImportReport, CoversStatusReport, ImportReport, InitReport, ListReport, RemoveReport,
    RenameReport, StandardizeReport,
};

pub fn identify_text(r: &identify::Report) -> String {
    let mut text = r.summary();
    for error in &r.errors {
        let _ = write!(text, "\n{error}");
    }
    text + "\n"
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
    staged_footer(
        &mut out,
        r.mode,
        r.result.as_ref(),
        p.changes().max(1),
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
