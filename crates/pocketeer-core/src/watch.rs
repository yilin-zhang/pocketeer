//! A cheap "did anything change?" signal for the TUI: the modification times
//! of the folders whose listings matter (card Assets/Cores/Saves trees two
//! levels deep, the library database). Only `stat` calls, no file contents,
//! so it is safe to poll every couple of seconds even over USB.

use std::{
    hash::{Hash, Hasher},
    path::Path,
};

use crate::Pocket;

fn stamp(path: &Path, h: &mut impl Hasher) {
    if let Ok(meta) = std::fs::metadata(path)
        && let Ok(m) = meta.modified()
    {
        path.hash(h);
        m.hash(h);
        meta.len().hash(h);
    }
}

/// Hashes `dir` and its children up to `depth` levels down (directories only).
fn stamp_tree(dir: &Path, depth: u8, h: &mut impl Hasher) {
    stamp(dir, h);
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let p = entry.path();
        if p.is_dir() && !crate::fsutil::is_hidden(&entry.file_name()) {
            stamp_tree(&p, depth - 1, h);
        }
    }
}

/// Changes when a ROM, core, save or library row was added, removed or
/// renamed by anyone (this process included). Either side may be absent:
/// no card mounted, or no library yet.
pub fn fingerprint(pocket: Option<&Pocket>, library: Option<&Path>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if let Some(pocket) = pocket {
        let root = pocket.root();
        stamp_tree(&root.join("Assets"), 2, &mut h);
        stamp_tree(&root.join("Cores"), 1, &mut h);
        stamp_tree(&root.join("Saves"), 2, &mut h);
        stamp_tree(&root.join("Memories").join("Save States"), 2, &mut h);
        stamp(&crate::library::device::marker_path(pocket), &mut h);
    }
    if let Some(lib) = library {
        for f in ["library.sqlite", "library.sqlite-wal"] {
            stamp(&lib.join(f), &mut h);
        }
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{sd, write};

    #[test]
    fn changes_when_a_rom_is_added_or_renamed() {
        let (tmp, pocket) = sd();
        write(tmp.path(), "Assets/gba/common/a.gba", "1");
        let a = fingerprint(Some(&pocket), None);
        assert_eq!(
            a,
            fingerprint(Some(&pocket), None),
            "stable when nothing changed"
        );
        // Directory mtimes have 1 s granularity on some file systems; force a
        // different listing rather than relying on time passing.
        std::fs::rename(
            tmp.path().join("Assets/gba/common/a.gba"),
            tmp.path().join("Assets/gba/common/b.gba"),
        )
        .unwrap();
        let dir = tmp.path().join("Assets/gba/common");
        let f = std::fs::File::open(&dir).unwrap();
        let _ = f.set_modified(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000),
        );
        assert_ne!(a, fingerprint(Some(&pocket), None));
    }
}
