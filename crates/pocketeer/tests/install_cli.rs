//! End-to-end tests for install / update / remove against a
//! throwaway copy of the fixture SD card. All offline.

mod common;

use common::{Harness, build_zip};
use pocketeer_core::net::Cache;
use serde_json::Value;

/// GBA 1.3.0 release URL in the fixture inventory.
const GBA_URL: &str = "https://github.com/spiritualized1997/openFPGA-GBA/releases/download/v1.3.0/spiritualized.GBA_1.3.0.zip";

fn env() -> Harness {
    Harness::new().with_card().with_inventory()
}

const GBA_130: &str = r#"{"core":{"magic":"APF_VER_1","metadata":{"platform_ids":["gba"],"shortname":"GBA","author":"spiritualized","version":"1.3.0"},"cores":[{"name":"default","id":0,"filename":"gba.rev"}]}}"#;

fn gba_zip_files() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("Cores/spiritualized.GBA/core.json", GBA_130.as_bytes()),
        ("Cores/spiritualized.GBA/gba.rev", b"new-bitstream"),
        ("Platforms/gba.json", b"{\"platform\":{}}"),
        ("__MACOSX/._junk", b""),
    ]
}

fn plan_actions(plan: &Value) -> Vec<(String, String)> {
    plan["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            let kind = a["action"].as_str().unwrap().to_string();
            let path = a["dest"]
                .as_str()
                .or(a["path"].as_str())
                .or(a["zip_path"].as_str())
                .unwrap()
                .to_string();
            (kind, path)
        })
        .collect()
}

#[test]
fn install_from_file_preview_then_apply() {
    let env = env();
    let zip = env.write_zip("gba.zip", &gba_zip_files());
    let zip = zip.to_str().unwrap();

    // Dry run: plan shown, exit 0, nothing written.
    let (v, code) = env.json(&["cores", "install", "--from", zip, "--dry-run"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["mode"], "dry_run");
    assert_eq!(v["data"]["items"][0]["state"], "planned");
    assert!(v["data"]["items"][0]["result"].is_null());
    let plan = &v["data"]["items"][0]["plan"];
    assert_eq!(plan["core_id"], "spiritualized.GBA");
    assert_eq!(plan["installed_version"], "1.2.0");
    assert_eq!(plan["version"], "1.3.0");
    assert_eq!(plan["changed"], true);
    assert_eq!(
        plan_actions(plan),
        [
            (
                "install".to_string(),
                "Cores/spiritualized.GBA/core.json".to_string()
            ),
            (
                "install".to_string(),
                "Cores/spiritualized.GBA/gba.rev".to_string()
            ),
            ("install".to_string(), "Platforms/gba.json".to_string()),
            ("skip".to_string(), "__MACOSX/._junk".to_string()),
        ]
    );
    assert!(
        env.read("Cores/spiritualized.GBA/core.json")
            .unwrap()
            .contains("1.2.0")
    );
    assert!(!env.exists(".pocketeer"));

    // Apply.
    let (v, code) = env.json(&["cores", "install", "--from", zip]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["items"][0]["state"], "applied");
    let applied = &v["data"]["items"][0]["result"];
    assert_eq!(applied["installed"], 3);
    assert!(
        env.read("Cores/spiritualized.GBA/core.json")
            .unwrap()
            .contains("1.3.0")
    );
    assert_eq!(
        env.read("Cores/spiritualized.GBA/gba.rev").as_deref(),
        Some("new-bitstream")
    );
    assert!(env.exists("Platforms/gba.json"));
    assert!(!env.exists("__MACOSX"));
    assert!(
        !env.exists(".pocketeer/staging"),
        "undo folder is gone after success"
    );
    assert!(applied.get("backup_dir").is_none());

    // Idempotent: again → nothing to do, exit 0.
    let (v, code) = env.json(&["cores", "install", "--from", zip]);
    assert_eq!(code, 0);
    assert_eq!(v["data"]["items"][0]["plan"]["changed"], false);
    assert!(v["data"]["items"][0]["result"].is_null());
}

#[test]
fn update_all_uses_cached_download() {
    let env = env();
    Cache::new(&env.cache)
        .store_download(GBA_URL, &build_zip(&gba_zip_files()))
        .unwrap();

    let (v, code) = env.json(&["cores", "update", "--all"]);
    assert_eq!(code, 0, "{v}");
    let outcomes = v["data"]["items"].as_array().unwrap();
    assert_eq!(outcomes.len(), 1, "only GBA is outdated");
    assert_eq!(outcomes[0]["target"], "spiritualized.GBA");
    assert_eq!(outcomes[0]["plan"]["source"]["url"], GBA_URL);
    assert!(
        env.read("Cores/spiritualized.GBA/core.json")
            .unwrap()
            .contains("1.3.0")
    );
    assert!(
        env.read("Cores/agg23.Arduboy/core.json")
            .unwrap()
            .contains("0.9.0")
    );

    let (v, code) = env.json(&["cores", "list", "--outdated"]);
    assert_eq!(code, 0);
    assert!(v["data"]["cores"].as_array().unwrap().is_empty());
}

#[test]
fn update_without_cached_download_fails_offline_per_core() {
    let env = env();
    let (v, code) = env.json(&["cores", "update", "spiritualized.GBA"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "SOME_FAILED");
    assert_eq!(v["data"]["items"][0]["error"]["code"], "OFFLINE_NO_CACHE");
    // The one item failed and planned nothing: total counts failures plus changes.
    assert_eq!(v["error"]["details"]["failed"], 1);
    assert_eq!(v["error"]["details"]["total"], 1);
}

#[test]
fn update_unknown_core_is_reported() {
    let env = env();
    let (v, code) = env.json(&["cores", "update", "nobody.Here", "--dry-run"]);
    assert_eq!(code, 1);
    assert_eq!(v["data"]["items"][0]["error"]["code"], "CORE_NOT_INSTALLED");
}

#[test]
fn install_pinned_version_not_found() {
    let env = env();
    let (v, code) = env.json(&["cores", "install", "spiritualized.GBA@9.9", "--dry-run"]);
    assert_eq!(code, 1);
    let err = &v["data"]["items"][0]["error"];
    assert_eq!(err["code"], "VERSION_NOT_FOUND");
    assert_eq!(
        err["details"]["available"],
        serde_json::json!(["1.3.0", "1.2.0"])
    );
}

#[test]
fn remove_default_and_purge() {
    let env = env();
    env.write("Settings/spiritualized.GBA/x.bin", "s");
    env.write("Assets/gba/spiritualized.GBA/game.json", "g");
    env.write("Assets/gba/common/bios.bin", "b");

    let (v, code) = env.json(&["cores", "remove", "spiritualized.GBA", "--dry-run"]);
    assert_eq!(code, 0);
    let plan = &v["data"]["items"][0]["plan"];
    assert_eq!(
        plan_actions(plan),
        [
            ("remove".to_string(), "Cores/spiritualized.GBA".to_string()),
            (
                "remove".to_string(),
                "Settings/spiritualized.GBA".to_string()
            ),
        ]
    );

    let (v, code) = env.json(&["cores", "remove", "spiritualized.GBA", "--purge"]);
    assert_eq!(code, 0, "{v}");
    assert!(!env.exists("Cores/spiritualized.GBA"));
    assert!(!env.exists("Settings/spiritualized.GBA"));
    assert!(!env.exists("Assets/gba/spiritualized.GBA"));
    assert!(
        env.exists("Assets/gba/common/bios.bin"),
        "common assets are never removed"
    );
    assert!(
        !env.exists(".pocketeer/backup"),
        "nothing is kept on the card"
    );

    let (v, code) = env.json(&["cores", "remove", "spiritualized.GBA"]);
    assert_eq!(code, 1);
    assert_eq!(v["data"]["items"][0]["error"]["code"], "CORE_NOT_INSTALLED");
}

#[test]
fn human_dry_run_renders_plan() {
    let env = env();
    let zip = env.write_zip("gba.zip", &gba_zip_files());
    let out = env.run(
        false,
        &[
            "cores",
            "install",
            "--from",
            zip.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    insta::assert_snapshot!(stdout);
}
