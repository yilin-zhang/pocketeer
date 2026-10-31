//! End-to-end tests for `bios check`, `bios place` and `clean`.

mod common;

use common::Harness;
use serde_json::Value;

fn env() -> Harness {
    Harness::new().with_card()
}

fn files_of<'a>(v: &'a Value, core: &str) -> &'a Vec<Value> {
    v["data"]["cores"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["core_id"] == core)
        .unwrap()["files"]
        .as_array()
        .unwrap()
}

#[test]
fn bios_check_reports_missing_files() {
    let env = env();
    let (v, code) = env.json(&["bios", "check"]);
    assert_eq!(code, 0, "{v}");
    insta::assert_json_snapshot!(v);
}

#[test]
fn bios_check_human() {
    let env = env();
    let out = env.run(false, &["bios", "check"]);
    assert!(out.status.success());
    insta::assert_snapshot!(String::from_utf8(out.stdout).unwrap());
}

#[test]
fn bios_check_present_and_wrong_hash() {
    let env = env();
    env.write("Assets/gba/common/gba_bios.bin", "GBA BIOS");
    env.write("Assets/arduboy/common/games/game1.hex", "whatever");
    let (v, _) = env.json(&["bios", "check"]);
    assert!(
        files_of(&v, "spiritualized.GBA").is_empty(),
        "present files hidden by default"
    );
    let (v, _) = env.json(&["bios", "check", "--present"]);
    assert_eq!(files_of(&v, "spiritualized.GBA")[0]["status"], "present");
    assert_eq!(v["data"]["summary"]["present"], 2);
    // The old spelling still works for scripts.
    let (old, _) = env.json(&["bios", "check", "--all"]);
    assert_eq!(old["data"], v["data"]);

    env.write("Assets/gba/common/gba_bios.bin", "corrupt");
    let (v, _) = env.json(&["bios", "check", "spiritualized.GBA"]);
    assert_eq!(v["data"]["cores"].as_array().unwrap().len(), 1);
    assert_eq!(files_of(&v, "spiritualized.GBA")[0]["status"], "wrong_hash");
}

#[test]
fn bios_place_moves_root_files() {
    let env = env();
    env.write("gba_bios.bin", "GBA BIOS");
    env.write("game1.hex", "hex");

    let (v, _) = env.json(&["bios", "check"]);
    assert_eq!(v["data"]["summary"]["placeable"], 2);
    assert_eq!(
        files_of(&v, "spiritualized.GBA")[0]["status"],
        "found_at_root"
    );

    let (v, code) = env.json(&["bios", "place", "--dry-run"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["mode"], "dry_run");
    assert!(env.exists("gba_bios.bin"));

    let (v, code) = env.json(&["bios", "place"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["result"]["moved_files"], 2);
    assert!(!env.exists("gba_bios.bin"));
    assert!(env.exists("Assets/gba/common/gba_bios.bin"));
    assert!(env.exists("Assets/arduboy/common/games/game1.hex"));

    let (v, _) = env.json(&["bios", "check"]);
    assert_eq!(v["data"]["summary"]["missing"], 0);
    assert_eq!(v["data"]["summary"]["present"], 2);

    let (v, code) = env.json(&["bios", "place"]);
    assert_eq!(code, 0);
    assert!(v["data"]["actions"].as_array().unwrap().is_empty());
}

#[test]
fn clean_removes_junk_but_not_system_dirs_by_default() {
    let env = env();
    env.write(".Spotlight-V100/x", "s");
    env.write("Assets/__MACOSX/._a", "a");
    // .DS_Store is gitignored, so a fresh checkout has none in the fixture.
    env.write("Cores/.DS_Store", "x");

    let (v, code) = env.json(&["clean", "--dry-run"]);
    assert_eq!(code, 0, "{v}");
    assert!(env.exists("Cores/.DS_Store"));
    let paths: Vec<&str> = v["data"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        [
            "Assets/__MACOSX",
            "Cores/.DS_Store",
            "Cores/._agg23.Arduboy"
        ]
    );
    assert_eq!(
        v["data"]["system_dirs_present"],
        serde_json::json!([".Spotlight-V100"])
    );

    let (v, code) = env.json(&["clean"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["result"]["removed"], 3);
    assert!(!env.exists("Cores/.DS_Store"));
    assert!(!env.exists("Cores/._agg23.Arduboy"));
    assert!(!env.exists("Assets/__MACOSX"));
    assert!(env.exists(".Spotlight-V100/x"));
    assert!(env.exists("Cores/agg23.Arduboy/core.json"));

    let (v, code) = env.json(&["clean", "--system"]);
    assert_eq!(code, 0);
    assert_eq!(v["data"]["result"]["removed"], 1);
    assert!(!env.exists(".Spotlight-V100"));

    let (v, code) = env.json(&["clean"]);
    assert_eq!(code, 0);
    assert!(v["data"]["entries"].as_array().unwrap().is_empty());
}
