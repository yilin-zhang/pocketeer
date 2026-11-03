//! The `--json` contract, one snapshot per command.
//!
//! One scenario builds a card and a library through the binary, then every
//! command that changes something runs with `--dry-run` (so the plans are
//! what gets pinned) and every read-only command runs as is. Temp paths,
//! timestamps and cache ages are replaced before snapshotting, so a snapshot
//! only changes when a shape or a value pocketeer decides changes.

mod common;

use std::fs;

use common::{Harness, fixture};
use serde_json::Value;

/// Replaces what differs between runs: the temp dir, clock readings and
/// timestamped folder names.
fn normalize(v: &mut Value, tmp: &str) {
    const CLOCK: [&str; 10] = [
        "available_bytes",
        "total_bytes",
        "mtime",
        "created_at",
        "added_at",
        "age_secs",
        "checked_at",
        "fetched_at",
        "seconds",
        "nanos",
    ];
    match v {
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                if CLOCK.contains(&k.as_str()) && child.is_number() {
                    *child = Value::from(0);
                } else {
                    normalize(child, tmp);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| normalize(i, tmp)),
        Value::String(s) => {
            let mut out = s.replace(tmp, "[tmp]");
            // `.trash/<YYYYMMDD-HHMMSS>-<pid>/…`
            if let Some(at) = out.find(".trash/") {
                let start = at + ".trash/".len();
                let end = out[start..].find('/').map_or(out.len(), |e| start + e);
                out.replace_range(start..end, "[stamp]");
            }
            *s = out;
        }
        _ => {}
    }
}

struct Scenario {
    h: Harness,
}

impl Scenario {
    /// A card with two cores and junk, a library holding one verified game
    /// that is also on the card, a backed-up save and a backed-up Memory.
    fn new() -> Self {
        let h = Harness::new().with_card().with_inventory();
        let s = Scenario { h };
        s.ok(&["library", "init"]);
        s.h.write_src("Hello World (USA).fkh", "hello world");
        s.h.write_src("Unknown.xyz", "what is this");
        let src = s.h.src.to_str().unwrap().to_owned();
        s.ok(&[
            "library",
            "import",
            &src,
            "--platform",
            "fkh",
            "--dat",
            &common::dat(),
        ]);
        s.ok(&["card", "copy", "Hello World (USA).fkh"]);
        s.h.write("Saves/fkh/common/Hello World (USA).sav", "level 3");
        s.ok(&["saves", "backup"]);
        let sta = s.h.sd.join("Memories/Save States/cart_1.sta");
        fs::create_dir_all(sta.parent().unwrap()).unwrap();
        fs::copy(fixture("memories/cart_1.sta"), &sta).unwrap();
        s.ok(&["memories", "backup"]);
        // Something only the card has, for `card import`.
        s.h.write("Assets/gb/common/Card Only.gb", "card only");
        s
    }

    fn ok(&self, args: &[&str]) {
        let (v, code) = self.h.json(args);
        assert_eq!(code, 0, "{args:?}: {v}");
    }

    /// The envelope of `args`, normalized, with its exit code.
    fn run(&self, args: &[&str]) -> Value {
        let (mut v, code) = self.h.json(args);
        normalize(&mut v, self.h.tmp.path().to_str().unwrap());
        serde_json::json!({ "exit": code, "envelope": v })
    }
}

macro_rules! pin {
    ($s:expr, $name:literal, $($arg:expr),+ $(,)?) => {
        insta::assert_json_snapshot!($name, $s.run(&[$($arg),+]));
    };
}

#[test]
fn every_command_has_a_pinned_shape() {
    let s = Scenario::new();
    let zip = s.h.write_zip(
        "gba.zip",
        &[(
            "Cores/spiritualized.GBA/core.json",
            &fs::read(fixture("core-gba-1.3.0.json")).unwrap(),
        )],
    );
    let zip = zip.to_str().unwrap();
    let cover = fixture("covers/3x2.png");
    let sav = "Saves/fkh/common/Hello World (USA).sav";
    let sta = "Memories/Save States/cart_1.sta";
    let rom = "Assets/fkh/common/Hello World (USA).fkh";

    // Read-only.
    pin!(s, "status", "status");
    pin!(s, "bios_check", "bios", "check");
    pin!(s, "cores_list", "cores", "list");
    pin!(s, "cores_search", "cores", "search", "gba");
    pin!(s, "cores_info", "cores", "info", "spiritualized.GBA");
    pin!(s, "library_status", "library", "status");
    pin!(s, "library_list", "library", "list");
    pin!(s, "library_identify", "library", "identify");
    pin!(s, "covers_status", "covers", "status");
    pin!(s, "card_list", "card", "list");
    pin!(s, "card_check", "card", "check");
    pin!(s, "saves_list", "saves", "list");
    pin!(s, "saves_versions", "saves", "versions", sav);
    pin!(s, "memories_list", "memories", "list");

    // Plans.
    pin!(s, "bios_place", "bios", "place", "--dry-run");
    pin!(s, "clean", "clean", "--dry-run");
    pin!(
        s,
        "cores_install",
        "cores",
        "install",
        "--from",
        zip,
        "--dry-run"
    );
    pin!(s, "cores_update", "cores", "update", "--all", "--dry-run");
    pin!(
        s,
        "cores_remove",
        "cores",
        "remove",
        "agg23.Arduboy",
        "--dry-run"
    );
    let src = s.h.src.to_str().unwrap().to_owned();
    s.h.write_src("Quick Fox.fkh", "the quick brown fox");
    pin!(s, "library_import", "library", "import", &src, "--dry-run");
    pin!(
        s,
        "library_rename",
        "library",
        "rename",
        "Hello World (USA).fkh",
        "Hello",
        "--dry-run"
    );
    pin!(
        s,
        "library_remove",
        "library",
        "remove",
        "Hello World (USA).fkh",
        "--dry-run"
    );
    pin!(
        s,
        "library_standardize",
        "library",
        "standardize",
        "--dry-run"
    );
    pin!(
        s,
        "covers_import",
        "covers",
        "import",
        "Hello World (USA).fkh",
        &cover,
        "--dry-run"
    );
    pin!(
        s,
        "card_rename",
        "card",
        "rename",
        rom,
        "Hello",
        "--dry-run"
    );
    pin!(
        s,
        "card_copy",
        "card",
        "copy",
        "Hello World (USA).fkh",
        "--dry-run"
    );
    pin!(s, "card_import", "card", "import", "--dry-run");
    pin!(s, "saves_backup", "saves", "backup", "--dry-run");
    pin!(s, "saves_restore", "saves", "restore", sav, "--dry-run");
    pin!(s, "saves_trash", "saves", "trash", sav, "--dry-run");
    pin!(s, "memories_backup", "memories", "backup", "--dry-run");
    pin!(
        s,
        "memories_restore",
        "memories",
        "restore",
        sta,
        "--dry-run"
    );
    pin!(s, "memories_trash", "memories", "trash", sta, "--dry-run");

    // Failures carry the error object.
    pin!(
        s,
        "error_rom_not_found",
        "library",
        "rename",
        "nope.gba",
        "x",
        "--dry-run"
    );
    pin!(s, "error_core_not_found", "cores", "info", "nobody.Nothing");
}
