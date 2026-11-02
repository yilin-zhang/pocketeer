//! Explicit transfers and independent names, through the public CLI.
mod common;
use common::Harness;
use std::fs;

#[test]
fn copy_preview_then_apply_and_match_independent_names() {
    let env = Harness::new().with_card();
    fs::write(env.src.join("local.fkh"), "ROM bytes").unwrap();
    assert_eq!(env.json(&["library", "init"]).1, 0);
    assert_eq!(
        env.json(&[
            "library",
            "import",
            env.src.to_str().unwrap(),
            "--platform",
            "fkh"
        ])
        .1,
        0
    );
    let (preview, code) = env.json(&["card", "copy", "local.fkh", "--dry-run"]);
    assert_eq!(code, 0, "{preview}");
    assert_eq!(preview["data"]["files"].as_array().unwrap().len(), 1);
    assert!(!env.sd.join("Assets/fkh/common/local.fkh").exists());
    let (result, code) = env.json(&["card", "copy", "local.fkh"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["data"]["result"]["copied"], 1);
    assert_eq!(
        fs::read(env.sd.join("Assets/fkh/common/local.fkh")).unwrap(),
        b"ROM bytes"
    );
    let (rename, code) = env.json(&["library", "rename", "local.fkh", "new title"]);
    assert_eq!(code, 0, "{rename}");
    let (list, code) = env.json(&["card", "list"]);
    assert_eq!(code, 0, "{list}");
    let matched = list["data"]["comparison"]["local"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
    assert_eq!(matched["presence"], "both");
    assert_eq!(matched["name"], "new title.fkh");
    assert_eq!(matched["copies"][0]["path"], "Assets/fkh/common/local.fkh");
    assert_eq!(matched["copies"][0]["name_mismatch"], true);
    let (again, code) = env.json(&["card", "copy", "new title.fkh"]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["data"]["files"].as_array().unwrap().len(), 0);
}

#[test]
fn import_selects_an_exact_nested_path() {
    let env = Harness::new().with_card();
    assert_eq!(env.json(&["library", "init"]).1, 0);
    env.write("Assets/fkh/common/one/game.fkh", "one ROM");
    env.write("Assets/fkh/common/two/game.fkh", "two ROM");
    let (preview, code) = env.json(&[
        "card",
        "import",
        "Assets/fkh/common/two/game.fkh",
        "--dry-run",
    ]);
    assert_eq!(code, 0, "{preview}");
    assert_eq!(preview["data"]["summary"]["add"], 1);
    let (result, code) = env.json(&["card", "import", "Assets/fkh/common/two/game.fkh"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(
        fs::read(env.lib.join("roms/fkh/game.fkh")).unwrap(),
        b"two ROM"
    );
    assert_eq!(
        fs::read(env.sd.join("Assets/fkh/common/one/game.fkh")).unwrap(),
        b"one ROM"
    );
    let (listed, code) = env.json(&["card", "list"]);
    assert_eq!(code, 0, "{listed}");
    let imported = listed["data"]["roms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|rom| rom["path"] == "Assets/fkh/common/two/game.fkh")
        .unwrap();
    assert_eq!(imported["confidence"], "cached");
    assert!(imported["sha1"].is_string());
    let (missing, code) = env.json(&["card", "import", "game.fkh"]);
    assert_eq!(code, 1, "{missing}");
    assert_eq!(missing["data"]["not_found"][0], "game.fkh");
}
