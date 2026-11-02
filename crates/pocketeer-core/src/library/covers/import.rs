//! Validated local cover copies, independent of ROM names and source paths.

use super::CoverRecord;
use crate::library::{Library, db};
use crate::plan::{Applied, Plan};
use crate::{Error, Result, fsutil};
use image::{ImageFormat, ImageReader, Limits};
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::{
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct ImportPlan {
    pub game: db::Game,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
    pub previous: Option<CoverRecord>,
    #[serde(skip)]
    library_root: PathBuf,
    // The confirmation applies these exact validated bytes, even if the source moves.
    #[serde(skip)]
    image: Arc<[u8]>,
}

impl Plan for ImportPlan {
    fn changes(&self) -> usize {
        1
    }
}

fn invalid(source: &Path, reason: impl Into<String>) -> Error {
    Error::UnsupportedImage {
        path: source.to_path_buf(),
        reason: reason.into(),
    }
}

pub fn plan(lib: &Library, game_id: i64, source: &Path) -> Result<ImportPlan> {
    let game = db::get_game(lib.conn(), game_id)?.ok_or_else(|| Error::RomNotFound {
        spec: format!("game {game_id}"),
    })?;
    let file = fs::File::open(source).map_err(|e| Error::io(source, e))?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::io(source, e))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid(source, "Cover image is too large"));
    }
    let format = image::guess_format(&bytes)
        .map_err(|e| invalid(source, format!("Cannot identify image: {e}")))?;
    let extension = match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::WebP => "webp",
        _ => return Err(invalid(source, "Unsupported cover image format")),
    };
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|e| invalid(source, format!("Cannot decode cover image: {e}")))?;
    let digest = format!("{:x}", Sha1::digest(&bytes));
    Ok(ImportPlan {
        previous: db::cover_record(lib.conn(), game_id)?,
        game,
        source: source.to_path_buf(),
        destination: PathBuf::from("covers/imported")
            .join(game_id.to_string())
            .join(format!("{digest}.{extension}")),
        width: decoded.width(),
        height: decoded.height(),
        bytes: bytes.len(),
        library_root: lib.root().to_path_buf(),
        image: bytes.into(),
    })
}

#[derive(Debug, Serialize)]
pub struct ImportResult {
    pub path: PathBuf,
    pub replaced: bool,
    pub warnings: Vec<String>,
}

impl Applied for ImportResult {
    fn failed(&self) -> usize {
        0
    }
}

pub fn apply(lib: &mut Library, plan: &ImportPlan) -> Result<ImportResult> {
    if lib.root() != plan.library_root {
        return Err(Error::Stale {
            what: "the library".into(),
        });
    }
    let root = lib.root().to_path_buf();
    let destination = root.join(&plan.destination);
    let tx = lib
        .conn_mut()
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| Error::db(e, "begin cover import"))?;
    if db::get_game(&tx, plan.game.id)?.as_ref() != Some(&plan.game)
        || db::cover_record(&tx, plan.game.id)? != plan.previous
    {
        return Err(Error::Stale {
            what: format!("{} or its cover", plan.game.name),
        });
    }
    let created = match fs::read(&destination) {
        Ok(existing) if existing == plan.image.as_ref() => false,
        Ok(_) => return Err(Error::AlreadyExists { path: destination }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fsutil::write_new_via_temp(&destination, |out| {
                use std::io::Write;
                out.write_all(&plan.image)
                    .map_err(|e| Error::io(&destination, e))
            })?;
            true
        }
        Err(e) => return Err(Error::io(&destination, e)),
    };
    let saved = (|| -> Result<()> {
        db::set_imported_cover(&tx, plan.game.id, &plan.destination)?;
        tx.commit().map_err(|e| Error::db(e, "commit cover import"))
    })();
    if let Err(e) = saved {
        if created {
            let _ = fs::remove_file(&destination);
        }
        return Err(e);
    }
    let mut warnings = Vec::new();
    if let Some(previous) = &plan.previous
        && previous.imported
        && let Some(rel) = &previous.rel_path
        && rel != &plan.destination
        && rel.parent() == plan.destination.parent()
        && let Err(e) = fs::remove_file(root.join(rel))
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warnings.push(format!(
            "New cover saved; old image could not be removed: {e}"
        ));
    }
    Ok(ImportResult {
        path: plan.destination.clone(),
        replaced: plan.previous.as_ref().is_some_and(|p| p.rel_path.is_some()),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        library::{covers, rename},
        testutil::library_with,
    };

    fn image(path: &Path, value: u8) {
        image::RgbImage::from_pixel(3, 2, image::Rgb([value, 20, 30]))
            .save(path)
            .unwrap();
    }

    #[test]
    fn imported_copy_survives_source_removal_and_rom_rename_and_replaces_safely() {
        let (tmp, mut lib) = library_with(&[("game.gba", "ROM")]);
        let game = lib.list_games(None, None).unwrap().remove(0);
        let source = tmp.path().join("art.png");
        image(&source, 10);
        let original = fs::read(&source).unwrap();
        let preview = plan(&lib, game.id, &source).unwrap();
        assert_eq!((preview.width, preview.height), (3, 2));
        assert!(covers::path_for(&lib, game.id).unwrap().is_none());
        let result = apply(&mut lib, &preview).unwrap();
        let old = lib.root().join(&result.path);
        assert_eq!(fs::read(&old).unwrap(), original);
        fs::remove_file(&source).unwrap();
        let rename = rename::plan(&lib, None, "game.gba", "renamed").unwrap();
        rename::apply(&mut lib, &rename).unwrap();
        assert_eq!(covers::path_for(&lib, game.id).unwrap(), Some(old.clone()));

        fs::write(&source, b"invalid image").unwrap();
        assert_eq!(
            plan(&lib, game.id, &source).unwrap_err().code(),
            "UNSUPPORTED_IMAGE"
        );
        assert_eq!(fs::read(&old).unwrap(), original);
        image(&source, 90);
        let replacement = plan(&lib, game.id, &source).unwrap();
        let result = apply(&mut lib, &replacement).unwrap();
        assert!(result.replaced);
        assert!(!old.exists());
        assert_eq!(
            fs::read(lib.root().join(result.path)).unwrap(),
            fs::read(&source).unwrap()
        );
        assert!(
            db::cover_record(lib.conn(), game.id)
                .unwrap()
                .unwrap()
                .imported
        );
        // The first preview is out of date now.
        assert_eq!(apply(&mut lib, &preview).unwrap_err().code(), "STALE_PLAN");
    }

    #[test]
    fn failed_database_update_keeps_the_old_cover_and_removes_only_the_new_copy() {
        let (tmp, mut lib) = library_with(&[("game.gba", "ROM")]);
        let game = lib.list_games(None, None).unwrap().remove(0);
        let source = tmp.path().join("art.jpg");
        image(&source, 10);
        let first = plan(&lib, game.id, &source).unwrap();
        apply(&mut lib, &first).unwrap();
        let before = db::cover_record(lib.conn(), game.id).unwrap();
        image(&source, 90);
        let next = plan(&lib, game.id, &source).unwrap();
        lib.conn().execute_batch("CREATE TRIGGER reject_cover BEFORE UPDATE ON covers BEGIN SELECT RAISE(FAIL, 'test failure'); END;").unwrap();
        assert!(apply(&mut lib, &next).is_err());
        assert_eq!(db::cover_record(lib.conn(), game.id).unwrap(), before);
        assert_eq!(
            fs::read(lib.root().join(&first.destination)).unwrap(),
            first.image.as_ref()
        );
        assert!(!lib.root().join(next.destination).exists());
        assert!(source.exists());
    }
}
