//! End-to-end tests of the binary against the fixture SD card.
//!
//! Every run is `--offline` with its own cache directory, so nothing here
//! touches the network or the user's real cache. JSON output is
//! snapshot-tested: it's the contract agents rely on.

mod common;

use std::process::{Command, Output};

use common::{Harness, parse};
use serde_json::Value;

fn json_of(out: &Output) -> Value {
    parse(out).0
}

#[test]
fn cores_list_json_with_inventory() {
    let h = Harness::new().with_card().with_inventory();
    let out = h.raw(&["cores", "list", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    insta::assert_json_snapshot!(json_of(&out), {
        ".data.cores[].path" => "[path]",
        ".data.broken[].path" => "[path]",
        ".data.broken[].error.message" => "[message]",
        ".data.broken[].error.details.path" => "[path]",
        ".data.inventory.age_secs" => "[age]",
    });
}

#[test]
fn cores_list_without_inventory_still_works() {
    let h = Harness::new().with_card();
    let out = h.raw(&["cores", "list", "--json"]);
    assert!(out.status.success());
    let json = json_of(&out);
    assert_eq!(json["data"]["inventory"]["available"], false);
    assert_eq!(
        json["data"]["inventory"]["error"]["code"],
        "OFFLINE_NO_CACHE"
    );
    assert_eq!(json["data"]["cores"].as_array().unwrap().len(), 2);
    assert_eq!(json["data"]["cores"][0]["update_available"], false);
}

#[test]
fn cores_list_outdated_filters() {
    let h = Harness::new().with_card().with_inventory();
    let out = h.raw(&["cores", "list", "--outdated", "--json"]);
    let json = json_of(&out);
    let ids: Vec<&str> = json["data"]["cores"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["spiritualized.GBA"]);
}

#[test]
fn human_cores_list() {
    let h = Harness::new().with_card().with_inventory();
    let out = h.raw(&["cores", "list"]);
    assert!(out.status.success());
    insta::assert_snapshot!(
        h.text_of(&out)
            .replace(&regex_lite_age(&h.text_of(&out)), "[age]")
    );
}

/// The footer says "from cache, Ns old"; make it stable.
fn regex_lite_age(text: &str) -> String {
    text.split("from cache, ")
        .nth(1)
        .and_then(|rest| rest.split(" old").next())
        .unwrap_or("")
        .to_string()
}

#[test]
fn status_json() {
    let h = Harness::new().with_card();
    let out = h.raw(&["status", "--json"]);
    assert!(out.status.success());
    insta::assert_json_snapshot!(json_of(&out), {
        ".data.root" => "[root]",
        ".data.disk.total_bytes" => "[bytes]",
        ".data.disk.available_bytes" => "[bytes]",
    });
}

#[test]
fn search_json() {
    let h = Harness::new().with_card().with_inventory();
    let out = h.raw(&["cores", "search", "--json"]);
    assert!(out.status.success());
    insta::assert_json_snapshot!(json_of(&out), { ".data.inventory.age_secs" => "[age]" });
}

#[test]
fn search_filters_and_all() {
    let h = Harness::new().with_card().with_inventory();
    let ids = |args: &[&str]| -> Vec<String> {
        let mut full = vec!["cores", "search", "--json"];
        full.extend_from_slice(args);
        json_of(&h.raw(&full))["data"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].as_str().unwrap().to_string())
            .collect()
    };
    // Not installed, released only.
    assert_eq!(ids(&[]), ["jotego.jtbeta"]);
    assert_eq!(
        ids(&["--installed"]),
        ["agg23.Arduboy", "jotego.jtbeta", "spiritualized.GBA"]
    );
    // The old spelling still works for scripts.
    assert_eq!(ids(&["--all"]), ids(&["--installed"]));
    assert_eq!(ids(&["gba", "--installed"]), ["spiritualized.GBA"]);
    assert_eq!(
        ids(&["--platform", "arduboy", "--installed"]),
        ["agg23.Arduboy"]
    );
    assert!(ids(&["zzz"]).is_empty());
}

#[test]
fn search_without_sd_card_still_works() {
    let h = Harness::new().with_inventory();
    let out = h.raw(&["cores", "search", "--json"]);
    assert!(out.status.success());
    assert_eq!(json_of(&out)["data"]["hits"].as_array().unwrap().len(), 3);
}

#[test]
fn info_json_and_case_insensitive() {
    let h = Harness::new().with_card().with_inventory();
    let out = h.raw(&["cores", "info", "SPIRITUALIZED.gba", "--json"]);
    assert!(out.status.success());
    insta::assert_json_snapshot!(json_of(&out), {
        ".data.installed.path" => "[path]",
        ".data.inventory_status.age_secs" => "[age]",
    });
}

#[test]
fn info_unknown_core() {
    let h = Harness::new().with_card().with_inventory();
    let out = h.raw(&["cores", "info", "nobody.Nothing", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json_of(&out)["error"]["code"], "CORE_NOT_FOUND");
}

#[test]
fn human_info() {
    let h = Harness::new().with_card().with_inventory();
    let out = h.raw(&["cores", "info", "spiritualized.GBA"]);
    let text = h.text_of(&out);
    let age = regex_lite_age(&text);
    insta::assert_snapshot!(text.replace(&age, "[age]"));
}

#[test]
fn error_is_json_on_stdout_with_failure_exit() {
    let h = Harness::new();
    let out = h.raw(&["status", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let json = json_of(&out);
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "ROOT_NOT_FOUND");
    assert!(out.stderr.is_empty(), "JSON mode must keep stderr quiet");
}

#[test]
fn not_a_pocket_lists_missing_markers() {
    let h = Harness::new();
    std::fs::create_dir_all(&h.sd).unwrap();
    let out = h.raw(&["cores", "list", "--json"]);
    let json = json_of(&out);
    assert_eq!(json["error"]["code"], "NOT_A_POCKET");
    assert_eq!(
        json["error"]["details"]["missing"],
        serde_json::json!(["Analogue_Pocket.json", "Assets", "Cores"])
    );
}

#[test]
fn inventory_refresh_offline_uses_cache() {
    let h = Harness::new().with_inventory();
    let out = h.raw(&["inventory", "refresh", "--json"]);
    assert!(out.status.success());
    let json = json_of(&out);
    assert_eq!(json["data"]["cores"], 4);
    assert_eq!(json["data"]["platforms"], 3);
    assert_eq!(json["data"]["cores_source"]["source"], "cache");
}

#[test]
fn no_subcommand_is_usage_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_pocketeer"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "help goes to stderr when piped");
}

/// In JSON mode stdout always carries the envelope, even when clap stops us.
#[test]
fn usage_errors_are_json_envelopes() {
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_pocketeer"))
            .args(args)
            .output()
            .unwrap();
        (json_of(&out), out.status.code().unwrap(), out.stderr)
    };
    let (v, code, stderr) = run(&["--json"]);
    assert_eq!(code, 2);
    assert_eq!(v["ok"], false);
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["error"]["code"], "USAGE");
    assert!(v["data"].is_null());
    assert!(
        String::from_utf8_lossy(&stderr).contains("Usage:"),
        "help on stderr"
    );

    let (v, code, _) = run(&["--json", "cores", "frobnicate"]);
    assert_eq!(code, 2);
    assert_eq!(v["error"]["code"], "USAGE");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("frobnicate"),
        "{v}"
    );
    assert!(v["error"]["hint"].is_string());

    let (v, code, _) = run(&["--json", "library", "import", "/x"]);
    assert_eq!(code, 2);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--platform"),
        "{v}"
    );

    let (v, code, stderr) = run(&["--json", "--help"]);
    assert_eq!(code, 2);
    assert_eq!(v["error"]["code"], "USAGE");
    assert!(String::from_utf8_lossy(&stderr).contains("Usage:"));

    // Without --json, clap behaves as usual: help on stdout, exit 0.
    let out = Command::new(env!("CARGO_BIN_EXE_pocketeer"))
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("Usage:"));
    let out = Command::new(env!("CARGO_BIN_EXE_pocketeer"))
        .args(["cores", "frobnicate"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}

#[test]
fn docs_render_whole_tree_and_subtree() {
    let h = Harness::new();
    let out = h.raw(&["docs"]);
    assert!(out.status.success());
    let md = String::from_utf8(out.stdout).unwrap();
    for needle in [
        "# pocketeer CLI reference",
        "### `pocketeer cores install`",
        "### `pocketeer device sync`",
        "### `pocketeer library import`",
        "| `USAGE` |",
        "--dry-run",
        "--libretro-system",
        "--installed",
        "--present",
    ] {
        assert!(md.contains(needle), "missing {needle}");
    }
    // Every argument carries a description (the reference is generated from
    // them): in clap's long help an argument line is followed by a more
    // indented description line.
    let lines: Vec<&str> = md.lines().collect();
    let indent = |l: &str| l.len() - l.trim_start().len();
    let undocumented: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| {
            let t = l.trim_start();
            let is_arg = (t.starts_with("--") || t.starts_with("-") && t.contains(", --"))
                || (t.starts_with('<') || t.starts_with('[')) && (2..=6).contains(&indent(l));
            is_arg
                && !t.starts_with("--help")
                && !t.starts_with("-h,")
                && !t.starts_with("-V,")
                && !t.starts_with("--version")
                && lines
                    .get(i + 1)
                    .is_none_or(|n| n.trim().is_empty() || indent(n) <= indent(l))
        })
        .map(|(_, l)| *l)
        .collect();
    assert!(undocumented.is_empty(), "{undocumented:#?}");
    let (json, code) = {
        let out = h.raw(&["docs", "cores", "install", "--json"]);
        (json_of(&out), out.status.code().unwrap())
    };
    assert_eq!(code, 0);
    assert_eq!(
        json["data"]["commands"],
        serde_json::json!(["pocketeer cores install"])
    );
    assert!(
        json["data"]["markdown"]
            .as_str()
            .unwrap()
            .contains("--keep-platforms")
    );
    let out = h.raw(&["docs", "nope", "--json"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn config_set_get_unset_and_precedence() {
    // Not through the harness: it always passes --library, and this test is
    // about the config file being the fallback when the flag is absent.
    let h = Harness::new().with_card();
    let cfg = h.tmp.path().join("cfg/config.toml");
    let cfg_s = cfg.to_str().unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_pocketeer"))
            .args(["--offline", "--cache-dir", h.cache.to_str().unwrap()])
            .args(["--config", cfg_s])
            .args(args)
            .env_remove("POCKETEER_ROOT")
            .env_remove("POCKETEER_LIBRARY")
            .output()
            .unwrap();
        parse(&out)
    };
    let (v, code) = run(&["config", "path", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(v["data"]["exists"], false);

    let (v, code) = run(&["config", "set", "library", "/tmp/some lib", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["values"][0]["value"], "/tmp/some lib");
    assert!(cfg.is_file());

    let (v, _) = run(&["config", "get", "--json"]);
    let keys: Vec<&str> = v["data"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["library", "cache-dir", "root", "github-token"]);

    // The configured library is what `library status` looks for.
    let (v, code) = run(&["library", "status", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "LIBRARY_NOT_FOUND");
    assert_eq!(v["error"]["details"]["path"], "/tmp/some lib");

    let (v, code) = run(&["config", "set", "bogus", "1", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "CONFIG_KEY");

    let (v, _) = run(&["config", "unset", "library", "--json"]);
    assert!(v["data"]["values"][0]["value"].is_null());

    std::fs::write(&cfg, "library = [\n").unwrap();
    let (v, code) = run(&["status", "--json", "--root", h.sd.to_str().unwrap()]);
    assert_eq!(code, 1, "a broken config must not be ignored");
    assert_eq!(v["error"]["code"], "CONFIG_PARSE_ERROR");
}
