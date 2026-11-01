//! Human-readable renderings. Each `pub fn` returns the full text to print
//! for one command's report; the TUI reuses the plan renderers for its
//! confirm dialogs. Split by domain; this file holds only what several
//! domains share: tables, byte sizes, plan listings, the plan footer.
//!
//! `Mode::Interactive` text is shown inside the TUI: it never mentions CLI
//! flags or commands (the dialog's buttons say what happens next).

use std::{collections::BTreeMap, fmt::Write};

use pocketeer_core::{
    install::{
        Action, InstallPlan, UninstallPlan,
        plan::{RemoveReason, SkipReason},
    },
    inventory::InventoryStatus,
    library::{
        covers::FetchReport,
        import::{ImportAction, ImportSkip},
    },
    net::Source,
};

use crate::commands::Mode;

/// Longest plan listing before "… and N more".
pub(crate) const MAX_PLAN_LINES: usize = 60;

/// Writes at most [`MAX_PLAN_LINES`] of `lines`, then "… and N more".
pub(crate) fn plan_lines(out: &mut String, lines: impl IntoIterator<Item = String>) {
    let mut hidden = 0;
    for (i, line) in lines.into_iter().enumerate() {
        if i < MAX_PLAN_LINES {
            let _ = writeln!(out, "{line}");
        } else {
            hidden += 1;
        }
    }
    if hidden > 0 {
        let _ = writeln!(out, "  … and {hidden} more");
    }
}

pub(crate) fn mode_footer(out: &mut String, mode: Mode) {
    match mode {
        Mode::DryRun => {
            let _ = writeln!(out, "\nDry run: nothing was changed.");
        }
        Mode::Applied | Mode::Interactive => {}
    }
}

/// How every plan report ends: what applying did (`done` writes the ✓/✗
/// lines), else how to apply it, else `nothing` (the domain's own "nothing to
/// do" line; empty to say nothing).
pub(crate) fn staged_footer<R>(
    out: &mut String,
    mode: Mode,
    result: Option<&R>,
    changes: usize,
    nothing: &str,
    done: impl FnOnce(&mut String, &R),
) {
    match result {
        Some(r) => done(out, r),
        None if changes > 0 => mode_footer(out, mode),
        None => {
            if !nothing.is_empty() {
                let _ = writeln!(out, "{nothing}");
            }
        }
    }
}

/// "; skipped: 2 up to date, 1 not in DAT", or nothing.
pub(crate) fn skipped_summary(skipped: &BTreeMap<&str, usize>) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    format!(
        "; skipped: {}",
        skipped
            .iter()
            .map(|(k, v)| format!("{v} {k}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// "3 h ago" for a unix timestamp.
pub fn time_ago(t: i64) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let ago = (secs - t).max(0) as u64;
    format!("{} ago", duration(ago))
}

/// "3 h", "2 d" for a span of seconds.
pub fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

pub(crate) fn inventory_footer(out: &mut String, status: &InventoryStatus) {
    match (&status.source, &status.error) {
        (Some(src), _) => {
            let _ = writeln!(out, "\nInventory: {}", source(src));
        }
        (None, Some(err)) => {
            let _ = writeln!(out, "\nwarning: inventory unavailable: {}", err.message);
            if let Some(hint) = &err.hint {
                let _ = writeln!(out, "hint: {hint}");
            }
        }
        (None, None) => {}
    }
}

pub(crate) fn source(src: &Source) -> String {
    match src {
        Source::Network => "fetched just now".into(),
        Source::Cache {
            age_secs,
            fallback_reason: None,
        } => format!("from cache, {} old", duration(*age_secs)),
        Source::Cache {
            age_secs,
            fallback_reason: Some(reason),
        } => format!(
            "from cache, {} old (network failed: {reason})",
            duration(*age_secs)
        ),
    }
}

/// Renders aligned columns. Trailing empty cells don't leave trailing spaces.
pub(crate) fn table<const N: usize>(out: &mut String, header: [&str; N], rows: &[[String; N]]) {
    let mut widths: [usize; N] = header.map(str::len);
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let render = |cells: [&str; N]| {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            if i + 1 < N {
                line.extend(std::iter::repeat_n(' ', widths[i] - cell.chars().count()));
            }
        }
        line.trim_end().to_string()
    };
    let _ = writeln!(out, "{}", render(header));
    for row in rows {
        let _ = writeln!(out, "{}", render(row.each_ref().map(String::as_str)));
    }
}

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub(crate) fn file_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Core installation and removal plans.

pub(crate) fn install_plan(out: &mut String, plan: &InstallPlan, mode: Mode) {
    let transition = match &plan.installed_version {
        Some(cur) if !plan.changed => format!("{cur} (already installed)"),
        Some(cur) => format!("{cur} → {}", plan.version),
        None => format!("install {}", plan.version),
    };
    let _ = writeln!(out, "{}: {transition}", plan.core_id);
    if !plan.changed {
        let _ = writeln!(
            out,
            "  nothing to do{}",
            if mode == Mode::Interactive {
                ""
            } else {
                " (use --force to reinstall)"
            }
        );
        return;
    }
    let s = &plan.summary;
    let _ = writeln!(
        out,
        "  {} file(s), {} ({} new, {} replaced){}",
        s.install,
        bytes(s.bytes),
        s.install - s.overwrite,
        s.overwrite,
        if s.skip > 0 {
            format!(", {} skipped", s.skip)
        } else {
            String::new()
        }
    );
    if !plan.also_installs.is_empty() {
        let _ = writeln!(out, "  also installs: {}", plan.also_installs.join(", "));
    }
    if !plan.replaces.is_empty() {
        let _ = writeln!(out, "  replaces: {}", plan.replaces.join(", "));
    }
    if plan.requires_license {
        let _ = writeln!(
            out,
            "  note: this core needs a license file; see the author's instructions"
        );
    }
    actions(out, &plan.actions);
}

pub(crate) fn actions(out: &mut String, list: &[Action]) {
    plan_lines(
        out,
        list.iter().filter_map(|a| {
            Some(match a {
                Action::Install {
                    dest, overwrite, ..
                } => format!(
                    "  {} {}",
                    if *overwrite { "~" } else { "+" },
                    dest.display()
                ),
                Action::Remove { path, reason } => match reason {
                    RemoveReason::MovedFile { replaced_by } => {
                        format!("  - {}  (now at {})", path.display(), replaced_by.display())
                    }
                    RemoveReason::ReplacedCore { by } => {
                        format!("  - {}  (replaced by {by})", path.display())
                    }
                    RemoveReason::Uninstall => format!("  - {}", path.display()),
                },
                Action::MoveTree { from, to, .. } | Action::MoveFile { from, to, .. } => {
                    format!("  > {} → {}", from.display(), to.display())
                }
                Action::Skip { .. } => return None,
            })
        }),
    );
    let kept_platforms = list
        .iter()
        .filter(|a| {
            matches!(
                a,
                Action::Skip {
                    reason: SkipReason::KeepPlatformData,
                    ..
                }
            )
        })
        .count();
    if kept_platforms > 0 {
        let _ = writeln!(out, "  (kept {kept_platforms} existing Platforms/ file(s))");
    }
}

pub(crate) fn remove_plan(out: &mut String, plan: &UninstallPlan) {
    let _ = writeln!(
        out,
        "{}: remove {}{}",
        plan.core_id,
        plan.installed_version,
        if plan.purge {
            " (with assets and saves)"
        } else {
            ""
        }
    );
    if plan.actions.is_empty() {
        let _ = writeln!(out, "  nothing to remove");
    }
    actions(out, &plan.actions);
}

// ---------------------------------------------------------------------------
// Library import plans (library import, device import).

/// One planned import; `None` for skips (summarised separately).
pub(crate) fn import_action_line(a: &ImportAction) -> Option<String> {
    let ImportAction::Add {
        source,
        rom,
        game,
        verified,
        inner,
        suggested,
        ..
    } = a
    else {
        return None;
    };
    let src = file_name(source);
    Some(format!(
        "  + {rom}{}{}{}{}",
        if game != rom && !rom.starts_with(game.as_str()) {
            format!("  [{game}]")
        } else {
            String::new()
        },
        if src != *rom && inner.is_none() {
            format!("  (from: {src})")
        } else {
            String::new()
        },
        suggested
            .as_ref()
            .map(|s| format!("  → standard name: {s}"))
            .unwrap_or_default(),
        match (verified, inner) {
            (false, _) => "  (not in DAT)",
            (true, Some(_)) => "  (from zip)",
            (true, None) => "",
        }
    ))
}

/// Why an import skipped a file, worded the same everywhere.
pub(crate) fn import_skip_reason(r: &ImportSkip) -> &'static str {
    match r {
        ImportSkip::AlreadyInLibrary { .. } => "already in library",
        ImportSkip::NotInDat => "not in DAT",
        ImportSkip::NameConflict { .. } => "name conflict",
        ImportSkip::Duplicate { .. } => "duplicate",
        ImportSkip::Unreadable { .. } => "unreadable",
    }
}

/// Lists the adds of an import plan and tallies its skips.
pub(crate) fn import_actions<'a>(
    out: &mut String,
    list: impl Iterator<Item = &'a ImportAction> + Clone,
) -> BTreeMap<&'static str, usize> {
    plan_lines(out, list.clone().filter_map(import_action_line));
    let mut skipped = BTreeMap::new();
    for a in list {
        if let ImportAction::Skip { reason, .. } = a {
            *skipped.entry(import_skip_reason(reason)).or_default() += 1;
        }
    }
    skipped
}

/// Tells the user how to take the standard names an import plan suggests.
pub(crate) fn suggestion_footer<'a>(
    out: &mut String,
    mode: Mode,
    actions: impl Iterator<Item = &'a ImportAction>,
) {
    if mode == Mode::Interactive {
        // The TUI asked about the names in its own dialog already.
        return;
    }
    let n = actions
        .filter(|a| {
            matches!(
                a,
                ImportAction::Add {
                    suggested: Some(_),
                    ..
                }
            )
        })
        .count();
    if n > 0 {
        let _ = writeln!(
            out,
            "{n} file(s) have a standard name (→ above); add --standard-names to import them under it."
        );
    }
}

/// One line about the covers fetched after an import.
pub fn covers_summary(out: &mut String, covers: Option<&FetchReport>, mode: Mode) {
    if let Some(c) = covers {
        let _ = writeln!(
            out,
            "✓ covers: {} fetched, {} not on libretro, {} error(s){}",
            c.summary.fetched,
            c.summary.not_found,
            c.summary.errors,
            if c.unsupported_platforms.is_empty() {
                String::new()
            } else {
                format!(
                    " (no libretro mapping for {}; {})",
                    c.unsupported_platforms.join(", "),
                    if mode == Mode::Interactive {
                        "press f again and give the system name"
                    } else {
                        "`covers fetch --libretro-system`"
                    }
                )
            }
        );
    }
}

#[cfg(test)]
mod tests {
    use pocketeer_core::install::{InstallPlan, plan::Summary, zipfile::ZipSource};

    use super::*;

    fn unchanged_plan() -> InstallPlan {
        InstallPlan {
            core_id: "a.B".into(),
            version: "1.0".into(),
            installed_version: Some("1.0".into()),
            source: ZipSource::Url { url: String::new() },
            requires_license: false,
            also_installs: vec![],
            replaces: vec![],
            changed: false,
            summary: Summary::default(),
            actions: vec![],
        }
    }

    #[test]
    fn interactive_text_names_no_cli_flags() {
        let plan = unchanged_plan();
        let mut cli = String::new();
        install_plan(&mut cli, &plan, Mode::DryRun);
        assert!(cli.contains("--force"), "{cli}");
        let tui = crate::commands::cores::install_plan_text(&plan);
        assert!(!tui.contains("--"), "{tui}");
        assert!(tui.contains("nothing to do"), "{tui}");
    }

    #[test]
    fn plan_lines_truncate_after_the_limit() {
        let mut out = String::new();
        plan_lines(&mut out, (0..MAX_PLAN_LINES + 3).map(|i| format!("  {i}")));
        assert_eq!(out.lines().count(), MAX_PLAN_LINES + 1);
        assert!(out.ends_with("  … and 3 more\n"), "{out}");
        let mut out = String::new();
        plan_lines(&mut out, ["  a".to_string()]);
        assert_eq!(out, "  a\n");
    }

    #[test]
    fn staged_footer_ladder() {
        let footer = |mode, result: Option<&usize>, changes, nothing| {
            let mut out = String::new();
            staged_footer(&mut out, mode, result, changes, nothing, |out, n| {
                let _ = writeln!(out, "✓ {n}");
            });
            out
        };
        assert_eq!(footer(Mode::Applied, Some(&2), 2, "idle"), "✓ 2\n");
        assert_eq!(
            footer(Mode::DryRun, None, 2, "idle"),
            "\nDry run: nothing was changed.\n"
        );
        assert_eq!(footer(Mode::Interactive, None, 2, "idle"), "");
        assert_eq!(footer(Mode::Applied, None, 0, "idle"), "idle\n");
        assert_eq!(footer(Mode::Applied, None, 0, ""), "");
    }

    #[test]
    fn skipped_summary_wording() {
        assert_eq!(skipped_summary(&BTreeMap::new()), "");
        let mut m = BTreeMap::new();
        m.insert("up to date", 2);
        m.insert("not in DAT", 1);
        assert_eq!(skipped_summary(&m), "; skipped: 1 not in DAT, 2 up to date");
    }
}
