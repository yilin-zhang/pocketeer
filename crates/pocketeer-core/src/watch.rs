//! Directory metadata signals for external additions, removals and renames.
//! Nested ROM/save folders participate; file contents are never read.

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

fn stamp_tree(dir: &Path, h: &mut impl Hasher) {
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.file_type().is_dir() && !crate::fsutil::is_hidden(e.file_name()))
        .filter_map(std::result::Result::ok)
    {
        stamp(entry.path(), h);
    }
}

/// Changes when a ROM, core, save or library row was added, removed or
/// renamed by anyone (this process included). Either side may be absent:
/// no card mounted, or no library yet.
pub fn fingerprint(pocket: Option<&Pocket>, library: Option<&Path>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if let Some(pocket) = pocket {
        let root = pocket.root();
        stamp_tree(&root.join("Assets"), &mut h);
        stamp_tree(&root.join("Cores"), &mut h);
        stamp_tree(&root.join("Saves"), &mut h);
        stamp_tree(&root.join("GB Studio"), &mut h);
        stamp_tree(&root.join("Memories").join("Beta"), &mut h);
        stamp_tree(&root.join("Memories").join("Save States"), &mut h);
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
        write(tmp.path(), "Assets/gba/common/series/region/a.gba", "1");
        let a = fingerprint(Some(&pocket), None);
        assert_eq!(
            a,
            fingerprint(Some(&pocket), None),
            "stable when nothing changed"
        );
        // Directory mtimes have 1 s granularity on some file systems; force a
        // different listing rather than relying on time passing.
        std::fs::rename(
            tmp.path().join("Assets/gba/common/series/region/a.gba"),
            tmp.path().join("Assets/gba/common/series/region/b.gba"),
        )
        .unwrap();
        let dir = tmp.path().join("Assets/gba/common/series/region");
        let f = std::fs::File::open(&dir).unwrap();
        let _ = f.set_modified(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000),
        );
        assert_ne!(a, fingerprint(Some(&pocket), None));
    }
}
