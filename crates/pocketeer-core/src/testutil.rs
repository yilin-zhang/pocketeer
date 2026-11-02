//! Shared fixtures for unit tests inside this crate.

use std::{io::Write as _, path::Path};

use crate::{
    Pocket,
    catalog::Dat,
    fsutil::hash::HashCache,
    library::{
        Library,
        import::{self, ImportOptions},
    },
};

/// A minimal but complete `core.json` (spiritualized.GBA 1.3.0).
pub(crate) const CORE_JSON: &str = r#"{"core":{"magic":"APF_VER_1","metadata":{"platform_ids":["gba"],"shortname":"GBA","author":"spiritualized","version":"1.3.0"},"cores":[{"name":"default","id":0,"filename":"gba.rev"}]}}"#;

/// An empty but valid Pocket SD card in a temp dir.
pub(crate) fn sd() -> (tempfile::TempDir, Pocket) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("Assets")).unwrap();
    std::fs::create_dir_all(tmp.path().join("Cores")).unwrap();
    std::fs::write(tmp.path().join("Analogue_Pocket.json"), "{}").unwrap();
    let pocket = Pocket::open(tmp.path()).unwrap();
    (tmp, pocket)
}

/// Writes `root/rel`, creating parent folders.
pub(crate) fn write(root: &Path, rel: &str, body: impl AsRef<[u8]>) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// [`write`], then sets the file's mtime (seconds since the epoch).
pub(crate) fn write_with_mtime(root: &Path, rel: &str, body: impl AsRef<[u8]>, mtime: i64) {
    write(root, rel, body);
    crate::fsutil::set_mtime(&root.join(rel), mtime).unwrap();
}

/// Builds a zip in memory from `(name, contents)` pairs; a name ending in
/// `/` adds a directory entry.
pub(crate) fn build_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    use zip::write::SimpleFileOptions;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, contents) in files {
        if name.ends_with('/') {
            w.add_directory(name.trim_end_matches('/'), SimpleFileOptions::default())
                .unwrap();
        } else {
            w.start_file(*name, SimpleFileOptions::default()).unwrap();
            w.write_all(contents).unwrap();
        }
    }
    w.finish().unwrap().into_inner()
}

/// The test DAT (`tests/fixtures/roms/test.dat`): Hello World (USA),
/// Quick Fox (Europe), Two Discs (Japan) (Disc 1 + 2).
pub(crate) fn dat() -> Dat {
    Dat::parse(
        include_bytes!("../../../tests/fixtures/roms/test.dat"),
        Path::new("test.dat"),
    )
    .unwrap()
}

/// A fresh library at `<tmp>/lib` holding `files` (name, contents) imported
/// from `<tmp>/src` on platform `fkh` by copying.
pub(crate) fn library_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Library) {
    library_on("fkh", files)
}

/// Like [`library_with`], on `platform`.
pub(crate) fn library_on(platform: &str, files: &[(&str, &str)]) -> (tempfile::TempDir, Library) {
    let tmp = tempfile::tempdir().unwrap();
    let mut lib = Library::init(&tmp.path().join("lib")).unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    for (name, body) in files {
        write(&src, name, body);
    }
    let opts = ImportOptions::default();
    let plan = import::plan_folder(
        &lib,
        &src,
        Some(platform),
        &[],
        &opts,
        None,
        &mut HashCache::in_memory(),
        &mut |_| {},
    )
    .unwrap();
    import::apply_import(&mut lib, &plan, &mut |_| {}).unwrap();
    (tmp, lib)
}
