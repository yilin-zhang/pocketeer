//! End-to-end: register a card, subscribe, sync ROMs from the library,
//! import card-only ROMs, rename through to the card.

mod common;

use common::Harness;
use std::fs;

fn env() -> Harness {
    Harness::new().with_card().with_inventory()
}

#[test]
fn register_subscribe_sync() {
    let env = env();
    let src = env.tmp.path().join("roms");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("Hello World (USA).fkh"), "hello world").unwrap();
    let (_, code) = env.json(&["library", "init"]);
    assert_eq!(code, 0);
    let (v, code) = env.json(&[
        "library",
        "import",
        src.to_str().unwrap(),
        "--platform",
        "fkh",
    ]);
    assert_eq!(code, 0, "{v}");

    let (v, code) = env.json(&["device", "show"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "NO_DEVICE");

    let (v, code) = env.json(&["device", "add", "--name", "Pocket A"]);
    assert_eq!(code, 0, "{v}");
    let id = v["data"]["id"].as_str().unwrap().to_string();
    assert!(env.sd.join(".pocketeer/device.toml").is_file());

    let (v, code) = env.json(&[
        "device",
        "subscribe",
        "--platform",
        "fkh",
        "--core",
        "spiritualized.GBA",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["platforms"], serde_json::json!(["fkh"]));

    let (v, _) = env.json(&["device", "list"]);
    assert_eq!(v["data"]["current"], id);

    // Dry run: 1 ROM to copy; the core can't be downloaded offline → reported.
    let (v, code) = env.json(&["device", "sync", "--dry-run"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "SOME_FAILED");
    assert_eq!(v["data"]["summary"]["rom_changes"], 1);
    assert_eq!(v["data"]["cores"][0]["error"]["code"], "OFFLINE_NO_CACHE");

    // ROMs only: dry run, then apply.
    let (v, code) = env.json(&["device", "sync", "--roms-only", "--dry-run"]);
    assert_eq!(code, 0, "{v}");
    assert!(v["data"]["result"].is_null());
    let (v, code) = env.json(&["device", "sync", "--roms-only"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["result"]["roms"][0]["result"]["copied"], 1);
    assert!(
        env.sd
            .join("Assets/fkh/common/Hello World (USA).fkh")
            .is_file()
    );

    let (v, code) = env.json(&["device", "sync", "--roms-only", "Pocket A"]);
    assert_eq!(code, 0);
    assert_eq!(v["data"]["summary"]["rom_changes"], 0);
    let (v, _) = env.json(&["device", "show", &id]);
    assert!(v["data"]["last_sync"].is_number());

    let (v, code) = env.json(&["device", "forget", "pocket a"]);
    assert_eq!(code, 0, "{v}");
    let (v, _) = env.json(&["device", "list"]);
    assert!(v["data"]["devices"].as_array().unwrap().is_empty());
}

#[test]
fn orphans_import_and_renames_follow_through_to_the_card() {
    let env = env();
    let src = env.tmp.path().join("roms");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("old hello.fkh"), "hello world").unwrap();
    env.json(&["library", "init"]);
    let (v, code) = env.json(&[
        "library",
        "import",
        src.to_str().unwrap(),
        "--platform",
        "fkh",
    ]);
    assert_eq!(code, 0, "{v}");
    env.json(&["device", "add", "--name", "Pocket A"]);
    env.json(&["device", "subscribe", "--platform", "fkh"]);
    // Renamed while this card had no copy: the library remembers it.
    let (v, code) = env.json(&["library", "rename", "old hello", "Hello World (USA)"]);
    assert_eq!(code, 0, "{v}");
    assert!(v["data"]["card"].is_null());

    let w = |rel: &str, body: &str| {
        let p = env.sd.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    };
    // The card still has the old name (from an earlier sync): a pending rename, not an orphan.
    w("Assets/fkh/common/old hello.fkh", "hello world");
    w("Saves/fkh/common/old hello.sav", "save");
    w("Assets/fkh/common/Only Card.fkh", "only here");
    // Declared by the fixture's GBA core: a BIOS, not a ROM.
    w("Assets/gba/common/gba_bios.bin", "not really a bios");

    let (v, code) = env.json(&["device", "orphans"]);
    assert_eq!(code, 0, "{v}");
    let names: Vec<&str> = v["data"]["roms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Only Card.fkh"]);

    // Sync renames the card's copy (and its save) instead of copying.
    let (v, code) = env.json(&["device", "sync", "--roms-only", "--dry-run"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["roms"][0]["plan"]["summary"]["rename"], 1);
    assert_eq!(v["data"]["roms"][0]["plan"]["summary"]["copy"], 0);
    let (v, code) = env.json(&["device", "sync", "--roms-only"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["result"]["roms"][0]["result"]["renamed"], 1);
    assert!(
        env.sd
            .join("Assets/fkh/common/Hello World (USA).fkh")
            .is_file()
    );
    assert!(!env.sd.join("Assets/fkh/common/old hello.fkh").exists());
    assert!(
        env.sd
            .join("Saves/fkh/common/Hello World (USA).sav")
            .is_file()
    );
    assert!(
        env.sd.join("Assets/fkh/common/Only Card.fkh").is_file(),
        "no --delete"
    );

    // Import the orphan into the library; the card is left alone.
    let (v, code) = env.json(&["device", "import", "nope.fkh"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "SOME_FAILED");
    assert_eq!(v["data"]["not_found"], serde_json::json!(["nope.fkh"]));
    let (v, code) = env.json(&["device", "import", "--dry-run"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["parts"][0]["summary"]["add"], 1);
    // "Only Card" isn't in the test DAT: --dat-only skips it.
    let (v, code) = env.json(&["device", "import", "--dat", &common::dat(), "--dat-only"]);
    assert_eq!(code, 0, "{v}");
    let part = &v["data"]["parts"][0];
    assert_eq!(part["summary"]["add"], 0);
    assert_eq!(part["summary"]["skip"], 1);
    assert_eq!(part["actions"][0]["action"], "skip");
    assert_eq!(part["actions"][0]["reason"]["kind"], "not_in_dat");
    let (v, code) = env.json(&["device", "import"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["result"][0]["platform"], "fkh");
    assert_eq!(v["data"]["result"][0]["result"]["added"], 1);
    assert!(env.lib.join("roms/fkh/Only Card.fkh").is_file());
    assert!(env.sd.join("Assets/fkh/common/Only Card.fkh").is_file());
    let (v, _) = env.json(&["device", "orphans"]);
    assert!(v["data"]["roms"].as_array().unwrap().is_empty());

    // Rename with the card mounted: library and card move together.
    let (v, code) = env.json(&["library", "rename", "Only Card", "Card Game"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["card"]["renames"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"]["result"]["card_renamed"], 1);
    assert!(env.sd.join("Assets/fkh/common/Card Game.fkh").is_file());
    assert!(env.lib.join("roms/fkh/Card Game.fkh").is_file());
    let (v, code) = env.json(&["device", "sync", "--roms-only"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["summary"]["rom_changes"], 0);

    // The BIOS is still there and still not a stray.
    let (v, code) = env.json(&["device", "sync", "--roms-only", "--delete"]);
    assert_eq!(code, 0, "{v}");
    assert!(env.sd.join("Assets/gba/common/gba_bios.bin").is_file());
}
