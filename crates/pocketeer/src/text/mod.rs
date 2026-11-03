//! How things read: plans, what applying them did, reports, and the word
//! for each state ([`Label`]). The command line and the TUI show the same
//! text; what to do next (a flag, a key) is each front end's own to add, so
//! nothing here names a flag, a command or a key. One file per domain; this
//! one holds what they share: tables, sizes, times, plan listings.

use std::{collections::BTreeMap, fmt::Write};

use pocketeer_core::{
    ErrorInfo,
    card::install::{
        Action,
        plan::{RemoveReason, SkipReason},
    },
    library::{
        covers::{CoverStatus, FetchReport},
        import::{ImportAction, ImportSkip},
    },
    net::Source,
    plan::Failure,
};

use pocketeer_core::plan::Plan;

use crate::ops::{Batch, BatchState};

pub mod card;
pub mod cores;
pub mod library;
pub mod memories;
pub mod saves;

/// Longest plan listing before "… and N more".
const MAX_PLAN_LINES: usize = 60;

/// Writes at most [`MAX_PLAN_LINES`] of `lines`, then "… and N more".
fn plan_lines(out: &mut String, lines: impl IntoIterator<Item = String>) {
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

/// The word for a state, the same in every table and report that shows it.
pub trait Label {
    fn label(&self) -> &'static str;
}

/// A plan, a result or a report as people read it.
pub trait Text {
    fn text(&self) -> String;
}

/// "; skipped: 2 up to date, 1 not in DAT", or nothing.
fn skipped_summary(skipped: &BTreeMap<&str, usize>) -> String {
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

fn source(src: &Source) -> String {
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
pub fn table<const N: usize>(out: &mut String, header: [&str; N], rows: &[[String; N]]) {
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

fn file_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn actions(out: &mut String, list: &[Action]) {
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

// ---------------------------------------------------------------------------
// Library import plans (library import, device import).

/// One planned import; `None` for skips (summarised separately).
fn import_action_line(a: &ImportAction) -> Option<String> {
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
fn import_skip_reason(r: &ImportSkip) -> &'static str {
    match r {
        ImportSkip::AlreadyInLibrary { .. } => "already in library",
        ImportSkip::NotInDat => "not in DAT",
        ImportSkip::NameConflict { .. } => "name conflict",
        ImportSkip::Duplicate { .. } => "duplicate",
        ImportSkip::Unreadable { .. } => "unreadable",
    }
}

/// Lists the adds of an import plan and tallies its skips.
fn import_actions<'a>(
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

/// What a cover fetch did this run, as counts with what happened ("3",
/// "downloaded"); empty when it did nothing. Games already marked missing
/// are not news.
pub fn cover_news(c: &FetchReport) -> Vec<(usize, &'static str)> {
    let missing = c
        .items
        .iter()
        .filter(|i| i.status == CoverStatus::NotFound)
        .count();
    [
        (c.summary.fetched, "downloaded"),
        (missing, "not on libretro"),
        (c.summary.errors, "failed"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .collect()
}

/// One line about the covers fetched after an import.
pub fn covers_summary(out: &mut String, covers: Option<&FetchReport>) {
    if let Some(c) = covers {
        let news = cover_news(c);
        let _ = writeln!(
            out,
            "✓ covers: {}",
            if news.is_empty() {
                "nothing new".to_string()
            } else {
                news.iter()
                    .map(|(n, what)| format!("{n} {what}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
    }
}

/// One line per item that failed: which file, and why.
fn failure_lines(out: &mut String, failures: &[Failure]) {
    for f in failures {
        let _ = writeln!(out, "✗ {}: {}", f.path.display(), f.error.message);
    }
}

/// Lines that say an import's follow-up work (DAT check, covers) had trouble.
pub fn warning_lines(out: &mut String, warnings: &[ErrorInfo]) {
    for w in warnings {
        let _ = writeln!(out, "! {w}");
    }
}

// ---------------------------------------------------------------------------
// Batches.

/// Each item: why it couldn't be planned, or its plan and how applying it
/// went.
impl<P: Plan + Text> Text for Batch<P>
where
    P::Output: Text,
{
    fn text(&self) -> String {
        let mut out = String::new();
        for (i, item) in self.items.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            match &item.state {
                BatchState::Unplanned { error } => {
                    let _ = writeln!(
                        out,
                        "{}: error [{}] {}",
                        item.target, error.code, error.message
                    );
                }
                BatchState::Planned { plan } => out.push_str(&plan.text()),
                BatchState::Applied { plan, result } => {
                    out.push_str(&plan.text());
                    for line in result.text().lines() {
                        let _ = writeln!(out, "  {line}");
                    }
                }
                BatchState::Failed { plan, error } => {
                    out.push_str(&plan.text());
                    let _ = writeln!(out, "  ✗ failed [{}] {}", error.code, error.message);
                }
            }
        }
        out
    }
}
