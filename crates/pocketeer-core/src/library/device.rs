//! Devices: SD cards the library knows. A card carries a small marker file
//! (`.pocketeer/device.toml`) with its id, so it is recognised wherever it is
//! mounted; the library remembers what each card subscribes to.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Error, Pocket, Result, fsutil};

use super::{Library, db};

pub const MARKER_FILE: &str = "device.toml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceMarker {
    pub id: String,
    pub name: String,
}

pub fn marker_path(pocket: &Pocket) -> std::path::PathBuf {
    pocket.pocketeer_dir().join(MARKER_FILE)
}

pub fn read_marker(pocket: &Pocket) -> Result<Option<DeviceMarker>> {
    let path = marker_path(pocket);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(&path, e)),
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| Error::ConfigParse {
            path,
            message: e.to_string(),
        })
}

pub fn write_marker(pocket: &Pocket, marker: &DeviceMarker) -> Result<()> {
    let text = toml::to_string(marker).expect("marker serializes");
    crate::fsutil::write_atomic(&marker_path(pocket), text.as_bytes())
}

/// 16 hex chars from the OS RNG (falls back to a time/pid hash).
pub fn new_id() -> String {
    let mut bytes = [0u8; 8];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_ok();
    if !ok {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::time::SystemTime::now().hash(&mut h);
        std::process::id().hash(&mut h);
        bytes = h.finish().to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub added_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<i64>,
    /// Platforms whose library ROMs go to `Assets/<platform>/common/`.
    pub platforms: Vec<String>,
    /// Cores kept installed at the latest inventory release.
    pub cores: Vec<String>,
}

pub(crate) const MIGRATION_DEVICES: &str = "
    CREATE TABLE devices (
        id        TEXT PRIMARY KEY,
        name      TEXT NOT NULL UNIQUE,
        added_at  INTEGER NOT NULL,
        last_sync INTEGER
    );
    CREATE TABLE device_platforms (
        device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
        platform  TEXT NOT NULL,
        PRIMARY KEY (device_id, platform)
    );
    CREATE TABLE device_cores (
        device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
        core_id   TEXT NOT NULL,
        PRIMARY KEY (device_id, core_id)
    );
";

fn load(conn: &Connection, id: &str) -> Result<Option<Device>> {
    let base = conn
        .query_row(
            "SELECT id, name, added_at, last_sync FROM devices WHERE id = ?1",
            params![id],
            |r| {
                Ok(Device {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    added_at: r.get(2)?,
                    last_sync: r.get(3)?,
                    platforms: Vec::new(),
                    cores: Vec::new(),
                })
            },
        )
        .optional()
        .map_err(|e| Error::db(e, "load device"))?;
    let Some(mut d) = base else {
        return Ok(None);
    };
    d.platforms = column(
        conn,
        "SELECT platform FROM device_platforms WHERE device_id = ?1 ORDER BY platform",
        id,
    )?;
    d.cores = column(
        conn,
        "SELECT core_id FROM device_cores WHERE device_id = ?1 ORDER BY core_id",
        id,
    )?;
    Ok(Some(d))
}

fn column(conn: &Connection, sql: &str, id: &str) -> Result<Vec<String>> {
    db::query_rows(conn, sql, params![id], "device column", |r| r.get(0))
}

pub fn list(lib: &Library) -> Result<Vec<Device>> {
    let ids: Vec<String> = db::query_rows(
        lib.conn(),
        "SELECT id FROM devices ORDER BY name COLLATE NOCASE",
        [],
        "list devices",
        |r| r.get(0),
    )?;
    ids.iter()
        .map(|id| load(lib.conn(), id).map(|d| d.expect("listed id exists")))
        .collect()
}

/// By id or name (case-insensitive).
pub fn get(lib: &Library, spec: &str) -> Result<Device> {
    if let Some(d) = load(lib.conn(), spec)? {
        return Ok(d);
    }
    let id: Option<String> = lib
        .conn()
        .query_row(
            "SELECT id FROM devices WHERE name = ?1 COLLATE NOCASE",
            params![spec],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| Error::db(e, "device by name"))?;
    match id {
        Some(id) => Ok(load(lib.conn(), &id)?.expect("just found")),
        None => Err(Error::DeviceNotFound {
            spec: spec.to_string(),
        }),
    }
}

/// The device whose marker is on the mounted card, if registered here.
pub fn current(lib: &Library, pocket: &Pocket) -> Result<Option<Device>> {
    match read_marker(pocket)? {
        Some(m) => load(lib.conn(), &m.id),
        None => Ok(None),
    }
}

/// Registers the mounted card. A card that already carries a marker keeps its
/// id (so moving a card between computers keeps it one device).
pub fn add(lib: &Library, pocket: &Pocket, name: &str) -> Result<Device> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::DeviceNotFound {
            spec: "(empty name)".into(),
        });
    }
    let marker = match read_marker(pocket)? {
        Some(m) => {
            if load(lib.conn(), &m.id)?.is_some() {
                return Err(Error::DeviceExists {
                    name: m.name,
                    id: m.id,
                });
            }
            DeviceMarker {
                id: m.id,
                name: name.to_string(),
            }
        }
        None => DeviceMarker {
            id: new_id(),
            name: name.to_string(),
        },
    };
    if get(lib, name).is_ok() {
        return Err(Error::DeviceExists {
            name: name.to_string(),
            id: String::new(),
        });
    }
    lib.conn()
        .execute(
            "INSERT INTO devices (id, name, added_at) VALUES (?1, ?2, ?3)",
            params![marker.id, marker.name, fsutil::now_secs()],
        )
        .map_err(|e| Error::db(e, "insert device"))?;
    write_marker(pocket, &marker)?;
    Ok(load(lib.conn(), &marker.id)?.expect("inserted"))
}

/// Forgets a device in the library. The marker on the card is left alone.
pub fn forget(lib: &Library, id: &str) -> Result<()> {
    let n = lib
        .conn()
        .execute("DELETE FROM devices WHERE id = ?1", params![id])
        .map_err(|e| Error::db(e, "delete device"))?;
    if n == 0 {
        return Err(Error::DeviceNotFound {
            spec: id.to_string(),
        });
    }
    Ok(())
}

pub fn subscribe(
    lib: &Library,
    id: &str,
    platforms: &[String],
    cores: &[String],
    add: bool,
) -> Result<Device> {
    let conn = lib.conn();
    for p in platforms {
        let sql = if add {
            "INSERT OR IGNORE INTO device_platforms (device_id, platform) VALUES (?1, ?2)"
        } else {
            "DELETE FROM device_platforms WHERE device_id = ?1 AND platform = ?2"
        };
        conn.execute(sql, params![id, p])
            .map_err(|e| Error::db(e, "device platforms"))?;
    }
    for c in cores {
        let sql = if add {
            "INSERT OR IGNORE INTO device_cores (device_id, core_id) VALUES (?1, ?2)"
        } else {
            "DELETE FROM device_cores WHERE device_id = ?1 AND core_id = ?2"
        };
        conn.execute(sql, params![id, c])
            .map_err(|e| Error::db(e, "device cores"))?;
    }
    load(conn, id)?.ok_or_else(|| Error::DeviceNotFound {
        spec: id.to_string(),
    })
}

pub fn touch_last_sync(lib: &Library, id: &str) -> Result<()> {
    lib.conn()
        .execute(
            "UPDATE devices SET last_sync = ?2 WHERE id = ?1",
            params![id, fsutil::now_secs()],
        )
        .map_err(|e| Error::db(e, "touch device"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testutil::sd;

    #[test]
    fn register_recognise_subscribe_forget() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::init(&tmp.path().join("lib")).unwrap();
        let (_sd, pocket) = sd();

        assert!(current(&lib, &pocket).unwrap().is_none());
        let d = add(&lib, &pocket, "Pocket A").unwrap();
        assert_eq!(d.id.len(), 16);
        assert_eq!(read_marker(&pocket).unwrap().unwrap().id, d.id);
        assert_eq!(current(&lib, &pocket).unwrap().unwrap().name, "Pocket A");
        assert_eq!(get(&lib, "pocket a").unwrap().id, d.id);
        assert_eq!(get(&lib, &d.id).unwrap().name, "Pocket A");
        assert_eq!(
            add(&lib, &pocket, "Again").unwrap_err().code(),
            "DEVICE_EXISTS"
        );

        let d = subscribe(
            &lib,
            &d.id,
            &["gba".into(), "gb".into()],
            &["x.Core".into()],
            true,
        )
        .unwrap();
        assert_eq!(d.platforms, ["gb", "gba"]);
        assert_eq!(d.cores, ["x.Core"]);
        let d = subscribe(&lib, &d.id, &["gb".into()], &[], false).unwrap();
        assert_eq!(d.platforms, ["gba"]);
        assert_eq!(list(&lib).unwrap().len(), 1);

        touch_last_sync(&lib, &d.id).unwrap();
        assert!(get(&lib, &d.id).unwrap().last_sync.is_some());

        forget(&lib, &d.id).unwrap();
        assert_eq!(get(&lib, &d.id).unwrap_err().code(), "DEVICE_NOT_FOUND");
        // Marker survives; re-adding adopts the same id.
        let again = add(&lib, &pocket, "Pocket A again").unwrap();
        assert_eq!(again.id, d.id);
    }

    #[test]
    fn ids_are_unique_hex() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
