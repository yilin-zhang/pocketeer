//! Text for `ops::card`.

use std::fmt::Write;

use pocketeer_core::{
    card::{
        Status,
        bios::{BiosReport, FileStatus, Note, PlacePlan, RootFile},
        clean::{CleanPlan, CleanResult, JunkKind},
        roms::Identity,
    },
    transfer::{
        Presence,
        rename::{CardRenamePlan, CardRenameResult},
        to_card::{CopyPlan, CopyResult},
    },
};

use super::{Label, Text, actions, bytes, failure_lines, source};
use crate::ops::card::{RefreshReport, RomsReport};

impl Text for Status {
    fn text(&self) -> String {
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
            format!(" ({} broken)", s.broken_cores)
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

impl Label for FileStatus {
    fn label(&self) -> &'static str {
        match self {
            FileStatus::Present => "present",
            FileStatus::Missing => "missing",
            FileStatus::WrongHash { .. } => "wrong md5",
            FileStatus::Unreadable { .. } => "unreadable",
            FileStatus::AtRoot {
                found: RootFile::File { .. },
            } => "at card root",
            FileStatus::AtRoot {
                found: RootFile::Zip { .. },
            } => "zipped at root",
        }
    }
}

/// What limits what is known of a core's files.
pub fn note(n: &Note) -> String {
    match n {
        Note::NoDataJson => "no data.json".into(),
        Note::Unreadable { error } => format!("unreadable: {}", error.message),
    }
}

impl Text for BiosReport {
    fn text(&self) -> String {
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
                    FileStatus::Missing => {
                        format!("  ✗ {}  ({req}, {})", f.path.display(), f.status.label())
                    }
                    FileStatus::WrongHash { actual_md5 } => format!(
                        "  ! {}  ({} {}, expected {})",
                        f.path.display(),
                        f.status.label(),
                        &actual_md5[..8.min(actual_md5.len())],
                        f.expected_md5
                            .as_deref()
                            .map(|m| &m[..8.min(m.len())])
                            .unwrap_or("?")
                    ),
                    FileStatus::Unreadable { error } => {
                        format!(
                            "  ! {}  ({}: {})",
                            f.path.display(),
                            f.status.label(),
                            error.message
                        )
                    }
                    FileStatus::AtRoot {
                        found: RootFile::File { path: root_file },
                    } => format!(
                        "  ↑ {}  ({req}, {}: {})",
                        f.path.display(),
                        f.status.label(),
                        root_file.display()
                    ),
                    FileStatus::AtRoot {
                        found: RootFile::Zip { zip, inner },
                    } => format!(
                        "  ↑ {}  ({req}, inside {} as {inner}; unzip it at the root)",
                        f.path.display(),
                        zip.display()
                    ),
                };
                let _ = writeln!(out, "{line}");
            }
            for n in &core.notes {
                let _ = writeln!(out, "  note: {}", note(n));
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

impl Text for PlacePlan {
    fn text(&self) -> String {
        let mut out = String::new();
        let list = &self.actions;
        if list.is_empty() {
            let _ = writeln!(out, "No root files to place.");
        } else {
            let _ = writeln!(out, "{} file(s) to move into place:", list.len());
            actions(&mut out, list);
        }
        for z in &self.zipped {
            if let FileStatus::AtRoot {
                found: RootFile::Zip { zip, inner },
            } = &z.status
            {
                let _ = writeln!(
                    out,
                    "  (skipped) {} is inside {}; unzip it at the root first",
                    inner,
                    zip.display()
                );
            }
        }
        out
    }
}

impl Text for CleanPlan {
    fn text(&self) -> String {
        let mut out = String::new();
        if self.entries.is_empty() {
            let _ = writeln!(out, "No macOS junk found.");
        } else {
            for e in &self.entries {
                let _ = writeln!(
                    out,
                    "  - {}  ({}, {})",
                    e.path.display(),
                    e.kind.label(),
                    bytes(e.bytes)
                );
            }
            let _ = writeln!(out, "{} item(s), {}", self.entries.len(), bytes(self.bytes));
        }
        if !self.system_dirs_present.is_empty() {
            let names: Vec<String> = self
                .system_dirs_present
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            let _ = writeln!(out, "macOS system folders left alone: {}", names.join(", "));
        }
        out
    }
}

impl Text for CleanResult {
    fn text(&self) -> String {
        let mut out = format!(
            "✓ removed {} item(s), {}\n",
            self.removed,
            bytes(self.bytes)
        );
        failure_lines(&mut out, &self.failed);
        out
    }
}

impl Label for JunkKind {
    fn label(&self) -> &'static str {
        match self {
            JunkKind::AppleDouble => "AppleDouble",
            JunkKind::DsStore => ".DS_Store",
            JunkKind::MacosxDir => "__MACOSX",
            JunkKind::SystemDir => "system dir",
            JunkKind::OldBackups => "old pocketeer backups",
        }
    }
}

impl Text for RefreshReport {
    fn text(&self) -> String {
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
    fn text(&self) -> String {
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

impl Text for CopyPlan {
    fn text(&self) -> String {
        let mut text = String::new();
        for file in &self.files {
            let _ = writeln!(
                text,
                "+ {} ({})",
                file.destination.display(),
                bytes(file.size)
            );
        }
        for path in &self.already_present {
            let _ = writeln!(text, "= already present: {}", path.display());
        }
        for warning in &self.warnings {
            let _ = writeln!(text, "Warning: {warning}");
        }
        if self.files.is_empty() {
            let _ = writeln!(text, "Nothing to copy.");
        }
        text
    }
}

impl Text for CopyResult {
    fn text(&self) -> String {
        let mut text = format!("Copied {} ROM(s)\n", self.copied);
        failure_lines(&mut text, &self.failed);
        for warning in &self.warnings {
            let _ = writeln!(text, "Warning: {warning}");
        }
        text
    }
}

impl Text for CardRenamePlan {
    fn text(&self) -> String {
        let mut text = String::new();
        let files =
            std::iter::once(("ROM", &self.rom)).chain(self.saves.iter().map(|s| ("save", s)));
        for (what, file) in files {
            let _ = writeln!(
                text,
                "{what}: {} → {}",
                file.from.display(),
                file.to.display()
            );
        }
        // A rename never touches Memories (see CardRenamePlan::saves).
        let _ = writeln!(
            text,
            "Warning: Memories stay unchanged; loading an existing save state may require the old ROM name."
        );
        text
    }
}

impl Text for CardRenameResult {
    fn text(&self) -> String {
        let mut text = format!("Renamed {} file(s)\n", self.renamed);
        for warning in &self.warnings {
            let _ = writeln!(text, "Warning: {warning}");
        }
        text
    }
}
