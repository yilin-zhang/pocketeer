mod common;
use common::Harness;

#[test]
fn nested_rom_identity_survives_cached_queries_and_explicit_checks() {
    let env = Harness::new().with_card();
    env.write("Assets/gba/common/folder/game.gba", "game contents");
    for (args, confidence) in [
        (vec!["card", "list"], "pending"),
        (vec!["card", "check"], "full"),
        (vec!["card", "list"], "cached"),
        (vec!["card", "check"], "sampled"),
        (vec!["card", "check", "--full"], "full"),
    ] {
        let (value, code) = env.json(&args);
        assert_eq!(code, 0, "{value}");
        let rom = value["data"]["roms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["path"] == "Assets/gba/common/folder/game.gba")
            .unwrap();
        assert_eq!(rom["confidence"], confidence);
        if confidence != "pending" {
            assert_eq!(rom["sha1"].as_str().unwrap().len(), 40);
        }
    }
    assert!(env.sd.join(".pocketeer/rom-index.msgpack").is_file());
}
