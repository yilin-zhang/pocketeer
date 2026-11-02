//! Text for `ops::card`.

use std::fmt::Write;

use pocketeer_core::{
    card::{Status, bios::BiosReport, bios::FileStatus, clean::JunkKind, roms::Identity},
    names::RenameKind,
    plan::Plan,
    transfer::Presence,
};

use super::{Reader, Text, actions, bytes, failure_lines, source, staged_footer};
use crate::ops::card::{
    CleanReport, CopyReport, PlaceReport, RefreshReport, RenameReport, RomsReport,
};

impl Text for Status {
    fn text(&self, _: Reader) -> String {
        let s = self;
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
}

impl Text for BiosReport {
    fn text(&self, _: Reader) -> String {
        let r = self;
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
}

impl Text for PlaceReport {
    fn text(&self, reader: Reader) -> String {
        let r = self;
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
            reader,
            r.result.as_ref(),
            list.len(),
            "",
            |out, a| {
                let _ = writeln!(out, "  ✓ done: {} file(s) moved", a.moved_files);
            },
        );
        out
    }
}

impl Text for CleanReport {
    fn text(&self, reader: Reader) -> String {
        let r = self;
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
                if reader == Reader::Tui {
                    ""
                } else {
                    " (use --system to remove)"
                }
            );
        }
        staged_footer(
            &mut out,
            r.mode,
            reader,
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
                failure_lines(out, &res.failed);
            },
        );
        out
    }
}

impl Text for RefreshReport {
    fn text(&self, _: Reader) -> String {
        let r = self;
        format!(
            "Inventory: {} cores ({}), {} platforms ({})\nCache: {}\n",
            r.cores,
            source(&r.cores_source),
            r.platforms,
            source(&r.platforms_source),
            r.cache_dir.display()
        )
    }
}

/// Where a ROM is, said the way the TUI's WHERE column says it.
fn presence_label(presence: Presence) -> &'static str {
    match presence {
        Presence::Both => "library + card",
        Presence::Local => "library only",
        Presence::Card => "card only",
        Presence::Unknown => "not read yet",
    }
}

/// How sure pocketeer is of a card file's contents, in a word or two.
pub fn identity(identity: &Identity) -> &'static str {
    match identity {
        Identity::Pending => "not read yet",
        Identity::Cached { .. } => "cached",
        Identity::Sampled { .. } => "sampled",
        Identity::Full { .. } => "fully read",
        Identity::Error { .. } => "read error",
    }
}

impl Text for RomsReport {
    fn text(&self, _: Reader) -> String {
        let report = self;
        let mut text = String::new();
        for (rom, presence) in report.roms.iter().zip(&report.comparison.card) {
            let _ = writeln!(
                text,
                "{}  {}  {}",
                rom.rel_path.display(),
                presence_label(*presence),
                identity(&rom.identity)
            );
            if let Identity::Error { error } = &rom.identity {
                let _ = writeln!(text, "  {error}");
            }
        }
        for local in report.comparison.local.values() {
            if local.presence != Presence::Both {
                let _ = writeln!(
                    text,
                    "[{}] {}  {}",
                    local.platform,
                    local.name,
                    presence_label(local.presence)
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
}

impl Text for CopyReport {
    fn text(&self, reader: Reader) -> String {
        let report = self;
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
            reader,
            report.result.as_ref(),
            report.plan.changes(),
            "Nothing to copy.",
            |text, result| {
                let _ = writeln!(text, "Copied {} ROM(s)", result.copied);
                failure_lines(text, &result.failed);
                for warning in &result.warnings {
                    let _ = writeln!(text, "Warning: {warning}");
                }
            },
        );
        text
    }
}

impl Text for RenameReport {
    fn text(&self, reader: Reader) -> String {
        let report = self;
        let mut text = String::new();
        for file in &report.plan.moves {
            let _ = writeln!(
                text,
                "{}: {} → {}",
                match file.kind {
                    RenameKind::Rom => "ROM",
                    RenameKind::Save => "save",
                    RenameKind::SaveState => "save state",
                },
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
            reader,
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
}
