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

#[test]
fn trash_moves_library_copies_and_deletes_from_ap_after_archiving() {
    let env = Harness::new().with_card();
    assert_eq!(env.json(&["library", "init"]).1, 0);
    let rel = "Saves/gba/common/game.sav";
    env.write(rel, "never backed up");
    // From AP: archived into the library first, then deleted.
    let (preview, code) = env.json(&["saves", "trash", rel, "--card", "--dry-run"]);
    assert_eq!(code, 0, "{preview}");
    assert_eq!(preview["data"]["files"][0]["archive_first"], true);
    assert!(env.exists(rel));
    let (done, code) = env.json(&["saves", "trash", rel, "--card"]);
    assert_eq!(code, 0, "{done}");
    assert_eq!(done["data"]["result"]["archived"], 1);
    assert!(!env.exists(rel));
    // Everything the library has goes to its .trash/.
    let (done, code) = env.json(&["saves", "trash", rel]);
    assert_eq!(code, 0, "{done}");
    let trash = done["data"]["result"]["trash"].as_str().unwrap();
    assert!(env.lib.join(trash).is_dir());
    let (versions, _) = env.json(&["saves", "versions", rel]);
    assert_eq!(versions["data"]["versions"], serde_json::json!([]));
    // --card and --version together are a usage error.
    assert_eq!(
        env.json(&["saves", "trash", rel, "--card", "--version", "1"])
            .1,
        2
    );
}

#[test]
fn memories_list_back_up_and_trash_with_the_files_beside_them() {
    let env = Harness::new().with_card();
    assert_eq!(env.json(&["library", "init"]).1, 0);
    let sta = "Memories/Save States/cart_1.sta";
    let sav = "Memories/Save States/cart_1.sav";
    let mut state = vec![0u8; 600];
    state[28..32].copy_from_slice(b"KIRB");
    fs::create_dir_all(env.sd.join("Memories/Save States")).unwrap();
    fs::write(env.sd.join(sta), &state).unwrap();
    env.write(sav, "sram");
    env.write("Saves/gba/common/game.sav", "not a Memory");

    let (list, code) = env.json(&["memories", "list"]);
    assert_eq!(code, 0, "{list}");
    let found = &list["data"]["memories"];
    assert_eq!(found.as_array().unwrap().len(), 1);
    assert_eq!(found[0]["game"], "KIRB");
    assert_eq!(found[0]["cartridge"], true);
    assert_eq!(found[0]["backed_up"], false);

    // Only Memories: the cartridge save stays out of it.
    let (backup, code) = env.json(&["memories", "backup"]);
    assert_eq!(code, 0, "{backup}");
    assert_eq!(backup["data"]["result"]["pulled"], 2);
    assert!(!env.lib.join("saves/Saves/gba/common/game.sav").exists());
    assert_eq!(
        env.json(&["memories", "list"]).0["data"]["memories"][0]["backed_up"],
        true
    );

    let (trash, code) = env.json(&["memories", "trash", sta, "--card"]);
    assert_eq!(code, 0, "{trash}");
    assert_eq!(trash["data"]["result"]["trashed"], 2);
    assert!(!env.exists(sta) && !env.exists(sav));
    let (restore, code) = env.json(&["memories", "restore", sta]);
    assert_eq!(code, 0, "{restore}");
    assert_eq!(restore["data"]["result"]["pushed"], 2);
    assert_eq!(env.read(sav).as_deref(), Some("sram"));
}
