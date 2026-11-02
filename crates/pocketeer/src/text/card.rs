//! Text for `ops::card`.

use std::fmt::Write;

use pocketeer_core::{
    card::{Status, bios::BiosReport, bios::FileStatus, clean::JunkKind},
    plan::Plan,
};

use super::{
    actions, bytes, covers_summary, import_actions, skipped_summary, source, staged_footer,
    suggestion_footer, warning_lines,
};
use crate::ops::{
    Mode,
    card::{
        CleanReport, CopyReport, ImportReport as CardImportReport, PlaceReport, RefreshReport,
        RenameReport, RomsReport,
    },
};

pub fn status_text(s: &Status) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "SD card    {}", s.root.display());
    if let Some(product) = &s.product {
        let _ = writeln!(out, "Product    {product}");
    }
    match &s.firmware {
        Some(fw) => {
            let version = fw.version.as_deref().unwrap_or("unknown");
            match &fw.build_date {
                Some(date) => {
                    let _ = writeln!(out, "Firmware   {version} (built {date})");
                }
                None => {
                    let _ = writeln!(out, "Firmware   {version}");
                }
            }
        }
        None => {
            let _ = writeln!(out, "Firmware   unknown");
        }
    }
    if let Some(disk) = s.disk {
        let _ = writeln!(
            out,
            "Disk       {} free of {}",
            bytes(disk.available_bytes),
            bytes(disk.total_bytes)
        );
    }
    let broken = if s.broken_cores > 0 {
        format!(" ({} broken, see `pocketeer cores list`)", s.broken_cores)
    } else {
        String::new()
    };
    let _ = writeln!(out, "Cores      {} installed{broken}", s.installed_cores);
    for w in &s.warnings {
        let _ = writeln!(out, "warning: {}", w.message);
    }
    out
}

pub fn bios_report(r: &BiosReport) -> String {
    let mut out = String::new();
    for core in &r.cores {
        let _ = writeln!(out, "{}", core.core_id);
        if core.files.is_empty() && core.notes.is_empty() {
            let _ = writeln!(out, "  ✓ nothing missing");
        }
        for f in &core.files {
            let req = if f.required { "required" } else { "optional" };
            let line = match &f.status {
                FileStatus::Present => format!("  ✓ {}", f.path.display()),
                FileStatus::Missing => format!("  ✗ {}  ({req}, missing)", f.path.display()),
                FileStatus::WrongHash { actual_md5 } => format!(
                    "  ! {}  (wrong md5 {}, expected {})",
                    f.path.display(),
                    &actual_md5[..8.min(actual_md5.len())],
                    f.expected_md5
                        .as_deref()
                        .map(|m| &m[..8.min(m.len())])
                        .unwrap_or("?")
                ),
                FileStatus::Unreadable { error } => {
                    format!("  ! {}  (unreadable: {})", f.path.display(), error.message)
                }
                FileStatus::FoundAtRoot { root_file } => format!(
                    "  ↑ {}  ({req}, found at root: {} → `pocketeer bios place`)",
                    f.path.display(),
                    root_file.display()
                ),
                FileStatus::FoundAtRootZipped { zip, inner } => format!(
                    "  ↑ {}  ({req}, inside {} as {inner}; unzip it at the root)",
                    f.path.display(),
                    zip.display()
                ),
            };
            let _ = writeln!(out, "{line}");
        }
        for note in &core.notes {
            let _ = writeln!(out, "  note: {note}");
        }
    }
    let s = &r.summary;
    let _ = writeln!(
        out,
        "\n{} missing ({} placeable from root, {} zipped at root), {} wrong hash, {} present",
        s.missing, s.placeable, s.zipped_at_root, s.wrong_hash, s.present
    );
    out
}

pub fn place_report(r: &PlaceReport) -> String {
    let mut out = String::new();
    let list = &r.plan.actions;
    if list.is_empty() {
        let _ = writeln!(out, "No root files to place.");
    } else {
        let _ = writeln!(out, "{} file(s) to move into place:", list.len());
        actions(&mut out, list);
    }
    for z in &r.plan.zipped {
        if let FileStatus::FoundAtRootZipped { zip, inner } = &z.status {
            let _ = writeln!(
                out,
                "  (skipped) {} is inside {}; unzip it at the root first",
                inner,
                zip.display()
            );
        }
    }
    staged_footer(
        &mut out,
        r.mode,
        r.result.as_ref(),
        list.len(),
        "",
        |out, a| {
            let _ = writeln!(out, "  ✓ done: {} file(s) moved", a.moved_files);
        },
    );
    out
}

pub fn clean_report(r: &CleanReport) -> String {
    let mut out = String::new();
    if r.plan.entries.is_empty() {
        let _ = writeln!(out, "No macOS junk found.");
    } else {
        for e in &r.plan.entries {
            let kind = match e.kind {
                JunkKind::AppleDouble => "AppleDouble",
                JunkKind::DsStore => ".DS_Store",
                JunkKind::MacosxDir => "__MACOSX",
                JunkKind::SystemDir => "system dir",
                JunkKind::OldBackups => "old pocketeer backups",
            };
            let _ = writeln!(
                out,
                "  - {}  ({kind}, {})",
                e.path.display(),
                bytes(e.bytes)
            );
        }
        let _ = writeln!(
            out,
            "{} item(s), {}",
            r.plan.entries.len(),
            bytes(r.plan.bytes)
        );
    }
    if !r.plan.system_dirs_present.is_empty() {
        let names: Vec<String> = r
            .plan
            .system_dirs_present
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let _ = writeln!(
            out,
            "macOS system folders left alone: {}{}",
            names.join(", "),
            if r.mode == Mode::Interactive {
                ""
            } else {
                " (use --system to remove)"
            }
        );
    }
    staged_footer(
        &mut out,
        r.mode,
        r.result.as_ref(),
        r.plan.entries.len(),
        "",
        |out, res| {
            let _ = writeln!(
                out,
                "✓ removed {} item(s), {}",
                res.removed,
                bytes(res.bytes)
            );
            for f in &res.failed {
                let _ = writeln!(out, "✗ {}: {}", f.path.display(), f.error.message);
            }
        },
    );
    out
}

pub fn refresh_text(r: &RefreshReport) -> String {
    format!(
        "Inventory: {} cores ({}), {} platforms ({})\nCache: {}\n",
        r.cores,
        source(&r.cores_source),
        r.platforms,
        source(&r.platforms_source),
        r.cache_dir.display()
    )
}

pub fn roms_text(report: &RomsReport) -> String {
    let mut text = String::new();
    for (rom, presence) in report.roms.iter().zip(&report.comparison.card) {
        let _ = writeln!(
            text,
            "{}  {:?}  {:?}",
            rom.rel_path.display(),
            presence,
            rom.confidence
        );
        if let Some(error) = &rom.error {
            let _ = writeln!(text, "  {error}");
        }
    }
    for local in report.comparison.local.values() {
        if local.presence != pocketeer_core::transfer::Presence::Both {
            let _ = writeln!(
                text,
                "[{}] {}  {:?}",
                local.platform, local.name, local.presence
            );
        } else {
            for copy in local.copies.iter().filter(|c| c.name_mismatch) {
                let _ = writeln!(
                    text,
                    "[{}] {} ≠ {}",
                    local.platform,
                    local.name,
                    copy.path.display()
                );
            }
        }
    }
    for warning in &report.warnings {
        let _ = writeln!(text, "Warning: {warning}");
    }
    text
}

pub fn copy_report(report: &CopyReport) -> String {
    let mut text = String::new();
    for file in &report.plan.files {
        let _ = writeln!(
            text,
            "+ {} ({})",
            file.destination.display(),
            bytes(file.size)
        );
    }
    for path in &report.plan.already_present {
        let _ = writeln!(text, "= already present: {}", path.display());
    }
    for warning in &report.plan.warnings {
        let _ = writeln!(text, "Warning: {warning}");
    }
    staged_footer(
        &mut text,
        report.mode,
        report.result.as_ref(),
        report.plan.changes(),
        "Nothing to copy.",
        |text, result| {
            let _ = writeln!(text, "Copied {} ROM(s)", result.copied);
            for error in &result.failed {
                let _ = writeln!(text, "✗ {}", error.message);
            }
            for warning in &result.warnings {
                let _ = writeln!(text, "Warning: {warning}");
            }
        },
    );
    text
}

pub fn rename_report(report: &RenameReport) -> String {
    let mut text = String::new();
    for file in &report.plan.moves {
        let _ = writeln!(
            text,
            "{:?}: {} → {}",
            file.kind,
            file.from.display(),
            file.to.display()
        );
    }
    for warning in &report.plan.warnings {
        let _ = writeln!(text, "Warning: {warning}");
    }
    staged_footer(
        &mut text,
        report.mode,
        report.result.as_ref(),
        report.plan.changes(),
        "Nothing to rename.",
        |text, result| {
            let _ = writeln!(text, "Renamed {} file(s)", result.renamed);
            for warning in &result.warnings {
                let _ = writeln!(text, "Warning: {warning}");
            }
        },
    );
    text
}

pub fn import_report(r: &CardImportReport) -> String {
    let mut out = String::new();
    let p = &r.staged.plan;
    for part in &p.parts {
        let _ = writeln!(out, "[{}]", part.platform);
        let skipped = import_actions(&mut out, part.plan.actions.iter());
        if !skipped.is_empty() {
            let _ = writeln!(
                out,
                "  {}",
                skipped_summary(&skipped).trim_start_matches("; ")
            );
        }
    }
    for n in &r.not_found {
        let _ = writeln!(out, "✗ {}: not an unmatched ROM on this card", n.display());
    }
    let _ = writeln!(
        out,
        "{} ROM(s) to copy into the library (the card is not changed)",
        p.changes()
    );
    if r.staged.result.is_none() && p.changes() > 0 {
        suggestion_footer(
            &mut out,
            r.staged.mode,
            p.parts.iter().flat_map(|p| p.plan.actions.iter()),
        );
    }
    // Nothing is applied while a name is missing, so no footer then either.
    let changes = if r.not_found.is_empty() {
        p.changes()
    } else {
        0
    };
    staged_footer(
        &mut out,
        r.staged.mode,
        r.staged.result.as_ref(),
        changes,
        "Nothing to import.",
        |out, res| {
            for pi in res {
                let ir = &pi.result;
                let _ = writeln!(
                    out,
                    "✓ [{}] added {} ROM(s), {}{}",
                    pi.platform,
                    ir.added,
                    bytes(ir.bytes),
                    if ir.duplicates.is_empty() {
                        String::new()
                    } else {
                        format!("; {} already in the library, dropped", ir.duplicates.len())
                    }
                );
                for warning in &pi.warnings {
                    let _ = writeln!(out, "! {warning}");
                }
                for f in &ir.failed {
                    let _ = writeln!(out, "✗ {}: {}", f.source.display(), f.error.message);
                }
            }
            covers_summary(out, r.covers.as_ref(), r.staged.mode);
            warning_lines(out, &r.warnings);
        },
    );
    out
}
