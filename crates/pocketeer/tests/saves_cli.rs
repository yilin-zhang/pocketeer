//! End-to-end: saves sync / list / versions / restore.

mod common;

use common::Harness;
use std::fs;

fn env() -> Harness {
    Harness::new().with_card()
}

#[test]
fn pull_version_restore_push() {
    let env = env();
    env.json(&["library", "init"]);
    env.write("Saves/gba/common/Game.sav", "v1");
    let p = env.sd.join("Saves/gba/common/Game.sav");
    // Card times are set in the past so a later restore (stamped "now") wins.
    let set_mtime = |secs_ago: u64| {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
        fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    };
    set_mtime(120);

    let (v, code) = env.json(&["saves", "sync", "--dry-run"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["summary"]["pull"], 1);
    assert!(!env.lib.join("saves/Saves/gba/common/Game.sav").exists());

    let (v, code) = env.json(&["saves", "sync"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["result"]["pulled"], 1);
    assert!(env.lib.join("saves/Saves/gba/common/Game.sav").is_file());

    // A later save on the card (mtime must move past the 2 s tolerance).
    env.write("Saves/gba/common/Game.sav", "v2");
    set_mtime(60);
    let (v, code) = env.json(&["saves", "sync"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["result"]["versions_created"], 1);

    let (v, _) = env.json(&["saves", "list"]);
    assert_eq!(v["data"]["saves"][0]["versions"], 1);

    let (v, _) = env.json(&["saves", "versions", "Saves/gba/common/Game.sav"]);
    let id = v["data"]["versions"][0]["id"].as_i64().unwrap();

    let (v, code) = env.json(&["saves", "restore", &id.to_string(), "--dry-run"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["version"]["id"], id);
    assert!(v["data"]["result"].is_null());
    let (v, code) = env.json(&["saves", "restore", &id.to_string()]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["result"]["restored"], true);
    assert_eq!(
        fs::read_to_string(env.lib.join("saves/Saves/gba/common/Game.sav")).unwrap(),
        "v1"
    );

    // Pull-only refuses to push the restored copy; push does it.
    let (v, code) = env.json(&["saves", "sync", "--pull"]);
    assert_eq!(code, 0);
    assert_eq!(v["data"]["summary"]["excluded"], 1);
    let (v, code) = env.json(&["saves", "sync", "--push"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["result"]["pushed"], 1);
    assert_eq!(fs::read_to_string(&p).unwrap(), "v1");

    let (v, code) = env.json(&["saves", "restore", "4242"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "SAVE_VERSION_NOT_FOUND");
}
