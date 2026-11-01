mod common;
use common::Harness;
use pocketeer_core::fsutil::set_mtime;
use std::fs;

#[test]
fn backup_conflict_choice_and_explicit_history_restore() {
    let env = Harness::new().with_card();
    assert_eq!(env.json(&["library", "init"]).1, 0);
    let rel = "Saves/gba/common/game.sav";
    env.write(rel, "AP first");
    set_mtime(&env.sd.join(rel), 100).unwrap();
    let (result, code) = env.json(&["saves", "backup"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["data"]["result"]["pulled"], 1);
    env.write(rel, "AP second");
    set_mtime(&env.sd.join(rel), 200).unwrap();
    assert_eq!(env.json(&["saves", "backup"]).1, 0);
    let backup = env.lib.join("saves").join(rel);
    fs::write(&backup, "PC version").unwrap();
    set_mtime(&backup, 300).unwrap();
    let (conflict, code) = env.json(&["saves", "backup"]);
    assert_eq!(code, 1, "{conflict}");
    assert_eq!(conflict["data"]["summary"]["conflicts"], 1);
    assert_eq!(fs::read(env.sd.join(rel)).unwrap(), b"AP second");
    let (chosen, code) = env.json(&["saves", "backup", "--conflicts", "computer"]);
    assert_eq!(code, 0, "{chosen}");
    assert_eq!(chosen["data"]["result"]["pushed"], 1);
    assert_eq!(fs::read(env.sd.join(rel)).unwrap(), b"PC version");
    let (versions, code) = env.json(&["saves", "versions", rel]);
    assert_eq!(code, 0, "{versions}");
    let original = versions["data"]["versions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["mtime"] == 100)
        .unwrap();
    let id = original["id"].as_i64().unwrap().to_string();
    let (preview, code) = env.json(&["saves", "restore", rel, "--version", &id, "--dry-run"]);
    assert_eq!(code, 0, "{preview}");
    assert_eq!(fs::read(env.sd.join(rel)).unwrap(), b"PC version");
    let (restored, code) = env.json(&["saves", "restore", rel, "--version", &id]);
    assert_eq!(code, 0, "{restored}");
    assert_eq!(fs::read(env.sd.join(rel)).unwrap(), b"AP first");
    let (versions, _) = env.json(&["saves", "versions", rel]);
    assert!(
        versions["data"]["versions"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |v| fs::read(env.lib.join(v["stored_path"].as_str().unwrap())).unwrap()
                    == b"PC version"
            )
    );
}
