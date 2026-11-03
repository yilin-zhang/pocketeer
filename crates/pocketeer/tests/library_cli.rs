//! End-to-end tests for `library init / import / status / list / rename / remove`.

mod common;

use common::{Harness, build_zip, dat};

/// No card: library-only behaviour. The source folder holds the test DAT's ROMs.
fn env() -> Harness {
    let h = Harness::new();
    h.write_src("Hello World (USA).fkh", "hello world");
    h.write_src(
        "sub/quickfox.fkh",
        "The quick brown fox jumps over the lazy dog",
    );
    h.write_src("random.fkh", "not in dat");
    h.write_src(
        "discs.zip",
        build_zip(&[("Two Discs (Japan) (Disc 1).bin", b"abc")]),
    );
    h
}

#[test]
fn init_import_status_list() {
    let env = env();
    let roms = env.src.to_str().unwrap();

    let (v, code) = env.json(&["library", "status"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "LIBRARY_NOT_FOUND");

    let (v, code) = env.json(&["library", "init"]);
    assert_eq!(code, 0, "{v}");
    assert!(env.lib.join("library.sqlite").is_file());
    let (v, code) = env.json(&["library", "init"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "LIBRARY_EXISTS");

    // Preview, then apply.
    let (v, code) = env.json(&[
        "library",
        "import",
        roms,
        "--platform",
        "fkh",
        "--dat",
        &dat(),
        "--dry-run",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["summary"]["add"], 4);
    assert_eq!(v["data"]["summary"]["verified"], 3);
    assert!(!env.lib.join("roms/fkh").exists());

    let (v, code) = env.json(&[
        "library",
        "import",
        roms,
        "--platform",
        "fkh",
        "--dat",
        &dat(),
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["result"]["added"], 4);
    for name in [
        "Hello World (USA).fkh",
        "Quick Fox (Europe).fkh",
        "Two Discs (Japan) (Disc 1).bin",
        "random.fkh",
    ] {
        assert!(env.lib.join("roms/fkh").join(name).is_file(), "{name}");
    }

    let (v, code) = env.json(&["library", "status"]);
    assert_eq!(code, 0);
    assert_eq!(v["data"]["games"], 4);
    assert_eq!(v["data"]["platforms"][0]["verified"], 3);

    let (v, code) = env.json(&["library", "list", "--platform", "fkh", "quick"]);
    assert_eq!(code, 0);
    assert_eq!(v["data"]["games"][0]["name"], "Quick Fox (Europe)");
    assert_eq!(
        v["data"]["games"][0]["roms"][0]["rel_path"],
        "roms/fkh/Quick Fox (Europe).fkh"
    );

    let out = env.run(false, &["library", "status"]);
    let text = String::from_utf8(out.stdout)
        .unwrap()
        .replace(env.lib.to_str().unwrap(), "[lib]");
    insta::assert_snapshot!(text);

    // Re-import: nothing new.
    let (v, code) = env.json(&[
        "library",
        "import",
        roms,
        "--platform",
        "fkh",
        "--dat",
        &dat(),
    ]);
    assert_eq!(code, 0);
    assert_eq!(v["data"]["summary"]["add"], 0);
    assert_eq!(v["data"]["summary"]["skip"], 4);
}

#[test]
fn dat_only_import_makes_managed_copies() {
    let env = env();
    let roms = env.src.to_str().unwrap();
    env.json(&["library", "init"]);
    let (v, code) = env.json(&[
        "library",
        "import",
        roms,
        "--platform",
        "fkh",
        "--dat",
        &dat(),
        "--dat-only",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["result"]["added"], 3);
    assert!(env.lib.join("roms/fkh/Hello World (USA).fkh").exists());
    assert!(
        env.lib
            .join("roms/fkh/Two Discs (Japan) (Disc 1).bin")
            .exists(),
        "zip entries are extracted"
    );
    let (v, _) = env.json(&["library", "list", "hello"]);
    assert_eq!(
        v["data"]["games"][0]["roms"][0]["rel_path"],
        "roms/fkh/Hello World (USA).fkh"
    );
}

#[test]
fn dat_only_requires_dat() {
    let env = env();
    let out = env.run(
        true,
        &["library", "import", "x", "--platform", "fkh", "--dat-only"],
    );
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn rename_and_remove_without_a_card() {
    let env = env();
    let roms = env.src.to_str().unwrap();
    env.json(&["library", "init"]);
    let (v, code) = env.json(&["library", "import", roms, "--platform", "fkh"]);
    assert_eq!(code, 0, "{v}");

    // Dry run: extension inherited, game follows, no card part.
    let (v, code) = env.json(&[
        "library",
        "rename",
        "Hello World (USA)",
        "Hello (Europe)",
        "--dry-run",
    ]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["new_name"], "Hello (Europe).fkh");
    assert_eq!(v["data"]["new_game_name"], "Hello (Europe)");
    assert!(v["data"]["card"].is_null());
    assert!(env.lib.join("roms/fkh/Hello World (USA).fkh").is_file());

    let (v, code) = env.json(&["library", "rename", "Hello World (USA)", "Hello (Europe)"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["result"]["library_renamed"], 1);
    assert!(env.lib.join("roms/fkh/Hello (Europe).fkh").is_file());
    assert!(!env.lib.join("roms/fkh/Hello World (USA).fkh").exists());
    let (v, _) = env.json(&["library", "list", "hello"]);
    assert_eq!(v["data"]["games"][0]["name"], "Hello (Europe)");
    assert_eq!(
        v["data"]["games"][0]["roms"][0]["name"],
        "Hello (Europe).fkh"
    );
    assert_eq!(
        v["data"]["games"][0]["roms"][0]["rel_path"],
        "roms/fkh/Hello (Europe).fkh"
    );

    let (v, code) = env.json(&["library", "rename", "random.fkh", "Hello (Europe)"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "NAME_TAKEN");
    let (v, code) = env.json(&["library", "rename", "random.fkh", "a/b"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "INVALID_NAME");
    let (v, code) = env.json(&["library", "rename", "nope", "x"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "ROM_NOT_FOUND");

    let out = env.run(
        false,
        &[
            "library",
            "rename",
            "random.fkh",
            "Random Game",
            "--dry-run",
        ],
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("random.fkh  →  Random Game.fkh"), "{text}");

    // Remove: dry run, apply, trash.
    let (v, code) = env.json(&["library", "remove", "random.fkh", "quickfox", "--dry-run"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["items"].as_array().unwrap().len(), 2);
    let (v, code) = env.json(&["library", "remove", "random.fkh", "quickfox"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["result"]["removed"], 2);
    assert_eq!(v["data"]["result"]["games_removed"], 2);
    let trash = env.lib.join(v["data"]["trash"].as_str().unwrap());
    assert!(trash.join("roms/fkh/random.fkh").is_file());
    assert!(!env.lib.join("roms/fkh/random.fkh").exists());
    let (v, _) = env.json(&["library", "status"]);
    assert_eq!(v["data"]["games"], 2);
    let (v, code) = env.json(&["library", "remove", "random.fkh"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "ROM_NOT_FOUND");
}

/// A next step that names a flag is said after the report, for people only:
/// the JSON carries the facts it rests on, never the sentence.
#[test]
fn flag_hints_follow_the_text_and_stay_out_of_json() {
    let env = Harness::new();
    env.write_src("mystery.xyz", "what is this");
    assert_eq!(env.json(&["library", "init"]).1, 0);
    let src = env.src.to_str().unwrap();

    let out = env.raw(&["library", "import", src, "--dry-run"]);
    let text = String::from_utf8(out.stdout).unwrap();
    let (report, hint) = text
        .split_once("\n\n")
        .expect("a hint block after the report");
    assert!(report.contains("Left out, type unknown (1):"), "{text}");
    assert!(!report.contains("--platform"), "{text}");
    assert_eq!(
        hint.trim(),
        "Give the files of unknown type a platform with --platform <id>."
    );

    let (v, code) = env.json(&["library", "import", src, "--dry-run"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["unknown"].as_array().unwrap().len(), 1);
    assert!(!v.to_string().contains("--platform"), "{v}");
}
