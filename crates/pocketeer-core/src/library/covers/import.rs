//! Validated local cover copies, independent of ROM names and source paths.

use super::Cover;
use crate::library::{Library, db};
use crate::plan::{Applied, Pinned, Progress};
use crate::{Error, ErrorInfo, Result, fsutil};
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
pub struct CoverImportPlan {
    pub game: db::Game,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
    /// The cover the game had when planned.
    pub previous: Option<Cover>,
    #[serde(skip)]
    pinned: Pinned,
    // The confirmation applies these exact validated bytes, even if the source moves.
    #[serde(skip)]
    image: Arc<[u8]>,
}

impl CoverImportPlan {
    /// Whether the game already has an image this one takes the place of.
    pub fn replaces_cover(&self) -> bool {
        self.previous.as_ref().and_then(Cover::path).is_some()
    }
}

impl CoverImportPlan {
    pub fn changes(&self) -> usize {
        1
    }

    pub fn apply(&mut self, lib: &mut Library, _: Progress<'_>) -> Result<CoverImportResult> {
        self.pinned.check_library(lib)?;
        let root = lib.root().to_path_buf();
        let destination = root.join(&self.destination);
        let tx = lib
            .conn_mut()
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| Error::db(e, "begin cover import"))?;
        if db::get_game(&tx, self.game.id)?.as_ref() != Some(&self.game)
            || db::cover(&tx, self.game.id)? != self.previous
        {
            return Err(Error::Stale {
                what: format!("{} or its cover", self.game.name),
            });
        }
        let created = match fs::read(&destination) {
            Ok(existing) if existing == self.image.as_ref() => false,
            Ok(_) => return Err(Error::AlreadyExists { path: destination }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fsutil::write_new_via_temp(&destination, |out| {
                    use std::io::Write;
                    out.write_all(&self.image)
                        .map_err(|e| Error::io(&destination, e))
                })?;
                true
            }
            Err(e) => return Err(Error::io(&destination, e)),
        };
        let saved = (|| -> Result<()> {
            db::set_imported_cover(&tx, self.game.id, &self.destination)?;
            tx.commit().map_err(|e| Error::db(e, "commit cover import"))
        })();
        if let Err(e) = saved {
            if created {
                let _ = fs::remove_file(&destination);
            }
            return Err(e);
        }
        let mut warnings = Vec::new();
        if let Some(Cover::Imported { path: rel }) = &self.previous
            && rel != &self.destination
            && rel.parent() == self.destination.parent()
            && let Err(e) = fs::remove_file(root.join(rel))
            && e.kind() != std::io::ErrorKind::NotFound
        {
            // The new cover is saved; the old image is merely left behind.
            warnings.push(Error::io(root.join(rel), e).to_info());
        }
        Ok(CoverImportResult {
            path: self.destination.clone(),
            replaced: self.replaces_cover(),
            warnings,
        })
    }
}

fn invalid(source: &Path, reason: impl Into<String>) -> Error {
    Error::UnsupportedImage {
        path: source.to_path_buf(),
        reason: reason.into(),
    }
}

pub fn plan(lib: &Library, game_id: i64, source: &Path) -> Result<CoverImportPlan> {
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
    Ok(CoverImportPlan {
        previous: db::cover(lib.conn(), game_id)?,
        game,
        source: source.to_path_buf(),
        destination: PathBuf::from("covers/imported")
            .join(game_id.to_string())
            .join(format!("{digest}.{extension}")),
        width: decoded.width(),
        height: decoded.height(),
        bytes: bytes.len(),
        pinned: Pinned::library(lib)?,
        image: bytes.into(),
    })
}

#[derive(Debug, Serialize)]
pub struct CoverImportResult {
    pub path: PathBuf,
    pub replaced: bool,
    pub warnings: Vec<ErrorInfo>,
}

impl Applied for CoverImportResult {
    fn failed(&self) -> usize {
        0
    }
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
        let (tmp, lib) = library_with(&[("game.gba", "ROM")]);
        let game = lib.list_games(None, None).unwrap().remove(0);
        let source = tmp.path().join("art.png");
        image(&source, 10);
        let original = fs::read(&source).unwrap();
        let mut preview = plan(&lib, game.id, &source).unwrap();
        assert_eq!((preview.width, preview.height), (3, 2));
        assert!(covers::path_for(&lib, game.id).unwrap().is_none());
        let result = preview
            .apply(&mut crate::testutil::reopen(&lib), &mut |_| {})
            .unwrap();
        let old = lib.root().join(&result.path);
        assert_eq!(fs::read(&old).unwrap(), original);
        fs::remove_file(&source).unwrap();
        let mut rename = rename::plan(&lib, None, "game.gba", "renamed").unwrap();
        rename
            .apply(&mut crate::testutil::reopen(&lib), &mut |_| {})
            .unwrap();
        assert_eq!(covers::path_for(&lib, game.id).unwrap(), Some(old.clone()));

        fs::write(&source, b"invalid image").unwrap();
        assert_eq!(
            plan(&lib, game.id, &source).unwrap_err().code(),
            "UNSUPPORTED_IMAGE"
        );
        assert_eq!(fs::read(&old).unwrap(), original);
        image(&source, 90);
        let mut replacement = plan(&lib, game.id, &source).unwrap();
        let result = replacement
            .apply(&mut crate::testutil::reopen(&lib), &mut |_| {})
            .unwrap();
        assert!(result.replaced);
        assert!(!old.exists());
        assert_eq!(
            fs::read(lib.root().join(result.path)).unwrap(),
            fs::read(&source).unwrap()
        );
        assert!(matches!(
            db::cover(lib.conn(), game.id).unwrap(),
            Some(Cover::Imported { .. })
        ));
        // The first preview is out of date now.
        assert_eq!(
            preview
                .apply(&mut crate::testutil::reopen(&lib), &mut |_| {})
                .unwrap_err()
                .code(),
            "STALE_PLAN"
        );
    }

    #[test]
    fn the_format_comes_from_the_bytes_not_the_file_name() {
        let (tmp, lib) = library_with(&[("game.gba", "ROM")]);
        let game = lib.list_games(None, None).unwrap().remove(0);
        let source = tmp.path().join("cover.jpg");
        image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 128]))
            .save_with_format(&source, image::ImageFormat::WebP)
            .unwrap();
        let mut preview = plan(&lib, game.id, &source).unwrap();
        let result = preview
            .apply(&mut crate::testutil::reopen(&lib), &mut |_| {})
            .unwrap();
        assert_eq!(result.path.extension().unwrap(), "webp");
        // Copied as is: the transparency survives.
        assert_eq!(
            fs::read(lib.root().join(&result.path)).unwrap(),
            fs::read(&source).unwrap()
        );
    }

    #[test]
    fn failed_database_update_keeps_the_old_cover_and_removes_only_the_new_copy() {
        let (tmp, lib) = library_with(&[("game.gba", "ROM")]);
        let game = lib.list_games(None, None).unwrap().remove(0);
        let source = tmp.path().join("art.jpg");
        image(&source, 10);
        let mut first = plan(&lib, game.id, &source).unwrap();
        first
            .apply(&mut crate::testutil::reopen(&lib), &mut |_| {})
            .unwrap();
        let before = db::cover(lib.conn(), game.id).unwrap();
        image(&source, 90);
        let mut next = plan(&lib, game.id, &source).unwrap();
        lib.conn().execute_batch("CREATE TRIGGER reject_cover BEFORE UPDATE ON covers BEGIN SELECT RAISE(FAIL, 'test failure'); END;").unwrap();
        assert!(
            next.apply(&mut crate::testutil::reopen(&lib), &mut |_| {})
                .is_err()
        );
        assert_eq!(db::cover(lib.conn(), game.id).unwrap(), before);
        assert_eq!(
            fs::read(lib.root().join(&first.destination)).unwrap(),
            first.image.as_ref()
        );
        assert!(!lib.root().join(next.destination).exists());
        assert!(source.exists());
    }
    #[test]
    fn cover_import_refuses_another_library_before_writing() {
        let (tmp, lib) = crate::testutil::library_on("gba", &[("game.gba", "ROM")]);
        let source = tmp.path().join("cover.png");
        image(&source, 90);
        let game = lib.list_games(None, None).unwrap().remove(0);
        let mut preview = plan(&lib, game.id, &source).unwrap();
        let (_other_dir, mut other) = crate::testutil::library_on("gba", &[("game.gba", "ROM")]);
        assert_eq!(
            preview.apply(&mut other, &mut |_| {}).unwrap_err().code(),
            "STALE_PLAN"
        );
        assert!(!other.root().join(&preview.destination).exists());
    }
}
