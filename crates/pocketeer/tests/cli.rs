//! Tests the command line from the outside, covering exit codes, output streams
//! and the shape of `--json`. The core's tests cover what the commands do.
//!
//! Every command gets `--library` and `--device` or needs no device, so no test
//! touches a mounted Pocket. None puts anything in the system's trash.

use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::Command,
    thread,
};

use pocketeer_core::{
    cores::testing::{core_json, data_json, index_file, zip_of},
    dat::testing::GBA_DAT,
    device::testing::make_empty_device,
    log::testing::logged_lines,
    os::testing::{put, put_at},
};
use serde_json::{Value, json};

struct Ran {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Ran {
    /// What was printed as JSON, without the times, which change.
    fn json(&self) -> Value {
        let mut value: Value = serde_json::from_str(&self.stdout).unwrap();
        if let Some(data) = value.get_mut("data").and_then(Value::as_array_mut) {
            for entry in data {
                if let Some(entry) = entry.as_object_mut() {
                    entry.remove("time");
                }
            }
        }
        value
    }
}

/// A library and a device in a temporary folder.
struct Setup {
    dir: tempfile::TempDir,
}

impl Setup {
    fn new() -> Self {
        let setup = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir(setup.library()).unwrap();
        make_empty_device(&setup.device());
        setup
    }

    fn library(&self) -> PathBuf {
        self.dir.path().join("library")
    }

    fn device(&self) -> PathBuf {
        self.dir.path().join("device")
    }

    fn in_library(&self, path: &str, bytes: &[u8]) -> PathBuf {
        put(&self.library(), path, bytes)
    }

    fn on_device(&self, path: &str, bytes: &[u8]) -> PathBuf {
        put(&self.device(), path, bytes)
    }

    /// Like [`Setup::on_device`], last written at `secs`.
    fn on_device_at(&self, path: &str, bytes: &[u8], secs: u64) -> PathBuf {
        put_at(&self.device(), path, bytes, secs)
    }

    /// Like [`Setup::in_library`], last written at `secs`.
    fn in_library_at(&self, path: &str, bytes: &[u8], secs: u64) -> PathBuf {
        put_at(&self.library(), path, bytes, secs)
    }

    /// A file apart from the library and the device.
    fn elsewhere(&self, name: &str, bytes: &[u8]) -> PathBuf {
        put(&self.dir.path().join("elsewhere"), name, bytes)
    }

    /// `pocketeer --library …`, for commands that need no device.
    fn command(&self) -> Command {
        let mut command = pocketeer_command();
        command.arg("--library").arg(self.library());
        command
    }

    /// `pocketeer --library … --device … <args>`.
    fn run(&self, args: &[&str]) -> Ran {
        let mut command = self.command();
        command.arg("--device").arg(self.device());
        run_and_capture(command.args(args))
    }

    fn log_lines(&self) -> Vec<Value> {
        logged_lines(&self.library().join("pocketeer.log"))
    }

    /// Makes the log impossible to write by putting a folder where it goes.
    fn break_the_log(&self) {
        fs::create_dir(self.library().join("pocketeer.log")).unwrap();
    }
}

fn pocketeer_command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pocketeer"))
}

fn run_and_capture(command: &mut Command) -> Ran {
    let output = command.output().unwrap();
    Ran {
        code: output.status.code().unwrap(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

/// Asserts a command couldn't start, with nothing on stdout, the reason on
/// stderr, and exit code 2.
#[track_caller]
fn assert_could_not_start(ran: &Ran, stderr: &str) {
    assert_eq!(ran.code, 2, "{}", ran.stderr);
    assert_eq!(ran.stdout, "");
    assert_eq!(ran.stderr, format!("{stderr}\n"));
}

/// The same for JSON, with the reason on stdout.
#[track_caller]
fn assert_json_could_not_start(ran: &Ran, reason: &str) {
    assert_eq!(ran.code, 2, "{}", ran.stderr);
    let value = ran.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["error"], reason);
    assert_eq!(value.get("data"), None);
}

/// Asserts a command succeeded and printed JSON with one entry, and returns the
/// entry.
#[track_caller]
fn assert_json_succeeded_with_one_entry(ran: &Ran) -> Value {
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    let value = ran.json();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], true);
    let entries = value["data"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    entries[0].clone()
}

#[test]
fn creates_default_library_in_home_dir() {
    let home = tempfile::tempdir().unwrap();
    let ran = run_and_capture(
        pocketeer_command()
            .env("HOME", home.path())
            .args(["orphan-saves", "library"]),
    );
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    assert!(home.path().join("Pocketeer Library/roms").is_dir());
}

/// Imports a game, puts it on the device, backs up its save and pushes an
/// older version back, checking what each step prints.
#[test]
fn game_and_save_round_trip() {
    let setup = Setup::new();
    let file = setup.elsewhere("mine.gba", b"mine");
    let ran =
        run_and_capture(
            setup
                .command()
                .args(["--json", "import", "game", file.to_str().unwrap()]),
        );
    let entry = assert_json_succeeded_with_one_entry(&ran);
    assert_eq!(entry["game"], "mine.gba");
    assert_eq!(entry["library"], "gba/mine.gba");

    let ran = setup.run(&["push", "game", "gba/mine.gba"]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    let save = setup.on_device_at("Saves/gba/common/mine.sav", b"played", 1_650_000_000);
    setup.on_device("Assets/gba/common/Here.gba", b"here");
    let cover = setup.in_library("covers/gba/mine.png", b"png");

    let ran = setup.run(&["games"]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    assert_eq!(
        ran.stdout,
        "LIBRARY       DEVICE        SAVE\n\
         gba/mine.gba  gba/mine.gba  new on the device\n\
         -             gba/Here.gba  -\n"
    );
    let ran = setup.run(&["--json", "games"]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        json!({
            "schema_version": 1,
            "ok": true,
            "data": [
                {
                    "platform": "gba",
                    "library": "gba/mine.gba",
                    "device": "gba/mine.gba",
                    "saves_kept": 0,
                    "new_save_on_device": true,
                    "cover": cover,
                },
                {
                    "platform": "gba",
                    "library": null,
                    "device": "gba/Here.gba",
                    "saves_kept": 0,
                    "new_save_on_device": false,
                    "cover": null,
                },
            ],
            "duplicates": [],
        })
    );

    // Without games named, it pulls each one the library lacks.
    setup.on_device("Assets/gba/common/Also.gba", b"also");
    let ran = setup.run(&["pull", "game"]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    assert_eq!(
        ran.stdout,
        "gba/Also.gba: pulled as gba/Also.gba\n\
         gba/Here.gba: pulled as gba/Here.gba\n"
    );
    assert_eq!(ran.stderr, "");

    let ran = setup.run(&["pull", "save", "gba/mine.gba"]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    setup.in_library_at("saves/gba/mine/old.sav", b"old", 1_600_000_000);
    let ran = setup.run(&["saves", "gba/mine.gba"]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    assert_eq!(
        ran.stdout,
        "VERSION                 SIZE\n\
         2022-04-15T05-20-00  6 bytes  newest\n\
         old                  3 bytes\n"
    );
    assert_eq!(
        setup.run(&["--json", "saves", "gba/mine.gba"]).json()["data"],
        json!([
            { "version": "2022-04-15T05-20-00", "size": 6, "newest": true },
            { "version": "old", "size": 3, "newest": false },
        ])
    );

    let ran = setup.run(&["push", "save", "gba/mine.gba", "--at", "old"]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    assert_eq!(ran.stdout, "gba/mine.gba: save old pushed\n");
    assert_eq!(fs::read(&save).unwrap(), b"old");
}

#[test]
fn continues_after_failure_and_exits_1() {
    let setup = Setup::new();
    setup.in_library("roms/gba/Mine.gba", b"mine");
    setup.on_device("Assets/gba/common/None.gba", b"none");
    setup.on_device("Saves/gba/common/None.sav", b"save");
    setup.on_device("Assets/gba/common/Mine.gba", b"mine");
    setup.on_device("Saves/gba/common/Mine.sav", b"save");
    let ran = setup.run(&["--json", "pull", "save", "gba/None.gba", "gba/Mine.gba"]);
    assert_eq!(ran.code, 1);
    let value = ran.json();
    assert_eq!(value["ok"], false);
    let ok: Vec<&Value> = value["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| &entry["ok"])
        .collect();
    assert_eq!(ok, [&json!(false), &json!(true)]);
}

#[test]
fn prints_status_as_text_and_json() {
    let setup = Setup::new();
    setup.on_device(
        "Analogue_Pocket.json",
        br#"{"firmware": {"runtime": {"name": "2.6", "build_date": "2026-06-16"}}}"#,
    );
    setup.on_device("Cores/a.B/core.json", b"{}");
    setup.on_device("Assets/gba/common/Mine.gba", b"game");
    let ran = setup.run(&["status"]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert_eq!(lines[0], format!("root      {}", setup.device().display()));
    assert_eq!(lines[1], "firmware  2.6 (built 2026-06-16)");
    assert!(lines[2].starts_with("space     "), "{}", lines[2]);
    assert_eq!(lines[3], "cores     1");
    assert_eq!(lines[4], "games     1");
    let value = setup.run(&["--json", "status"]).json();
    assert_eq!(value["ok"], true);
    let data = &value["data"];
    assert_eq!(data["root"], setup.device().display().to_string());
    assert_eq!(
        data["firmware"],
        json!({ "version": "2.6", "build_date": "2026-06-16" })
    );
    assert!(data["total_bytes"].as_u64().unwrap() > 0);
    assert!(data["available_bytes"].is_u64());
    assert_eq!(data["cores"], 1);
    assert_eq!(data["games"], 1);
}

mod could_not_start {
    use super::*;

    #[test]
    fn exits_2_with_empty_stdout_for_unknown_command() {
        let setup = Setup::new();
        let ran = setup.run(&["--json", "no-such-command"]);
        assert_eq!(ran.code, 2);
        assert_eq!(ran.stdout, "");
        assert!(ran.stderr.contains("no-such-command"), "{}", ran.stderr);
    }

    #[test]
    fn exits_2_without_command_outside_terminal() {
        let setup = Setup::new();
        let ran = setup.run(&[]);
        assert_could_not_start(
            &ran,
            "error: the terminal UI needs a terminal; give a command, or see --help",
        );
    }

    /// Every game named is checked before anything is done to any of them.
    #[test]
    fn exits_2_and_does_nothing_for_bad_game_names() {
        let setup = Setup::new();
        setup.in_library("roms/gba/Mine.gba", b"mine");
        setup.on_device("Assets/gba/common/One.gba", b"one");
        let how = "write a game as <platform>/<name>, as `pocketeer games` shows it";
        assert_could_not_start(
            &setup.run(&["push", "game", "Mine.gba"]),
            &format!("error: Mine.gba: {how}"),
        );
        assert_json_could_not_start(
            &setup.run(&["--json", "push", "game", "Mine.gba"]),
            &format!("Mine.gba: {how}"),
        );
        assert_could_not_start(
            &setup.run(&["pull", "game", "gba/One.gba", "gba/Nope.gba"]),
            "error: gba/Nope.gba: not on the device",
        );
        assert!(!setup.library().join("roms/gba/One.gba").exists());
        assert_json_could_not_start(
            &setup.run(&["--json", "push", "game", "gba/Mine.gba", "gba/Nope.gba"]),
            "gba/Nope.gba: not in the library",
        );
        assert!(!setup.device().join("Assets/gba/common/Mine.gba").exists());
    }

    #[test]
    fn exits_2_without_creating_missing_library() {
        let setup = Setup::new();
        let typo = setup.dir.path().join("typo");
        let ran = run_and_capture(
            pocketeer_command()
                .arg("--json")
                .arg("--library")
                .arg(&typo)
                .args(["orphan-saves", "library"]),
        );
        assert_json_could_not_start(&ran, &format!("no library at {}", typo.display()));
        assert!(!typo.exists());
    }

    #[test]
    fn exits_2_for_dir_that_is_not_device() {
        let setup = Setup::new();
        let ran = run_and_capture(
            setup
                .command()
                .arg("--json")
                .arg("--device")
                .arg(setup.library())
                .arg("games"),
        );
        let reason = format!(
            "{} is not an Analogue Pocket device",
            setup.library().display()
        );
        assert_json_could_not_start(&ran, &reason);
    }
}

mod duplicates_on_device {
    use super::*;

    /// One game with two copies, where only the later one has a save.
    fn two_copies() -> Setup {
        let setup = Setup::new();
        setup.in_library("roms/gba/Mine.gba", b"copy");
        setup.on_device("Assets/gba/common/One.gba", b"copy");
        setup.on_device("Assets/gba/common/RPG/Two.gba", b"copy");
        setup.on_device("Saves/gba/common/RPG/Two.sav", b"save");
        setup
    }

    const TWO_COPIES_WARNING: &str = "warning: gba/RPG/Two.gba is a second copy of a game; \
                                      it is left out and its save isn't synced\n";

    #[test]
    fn warns_of_duplicates_and_lists_them_in_json() {
        let setup = two_copies();
        let ran = setup.run(&["--json", "games"]);
        assert_eq!(ran.code, 0);
        assert_eq!(ran.stderr, TWO_COPIES_WARNING);
        assert_eq!(ran.json()["duplicates"], json!(["gba/RPG/Two.gba"]));
    }

    #[test]
    fn exits_2_for_command_naming_duplicate() {
        let setup = two_copies();
        for what in [
            "pull game",
            "pull save",
            "delete-on-device game",
            "delete-on-device save",
        ] {
            let mut args = vec!["--json"];
            args.extend(what.split(' '));
            args.push("gba/RPG/Two.gba");
            assert_json_could_not_start(
                &setup.run(&args),
                "gba/RPG/Two.gba: a second copy of a game, which is left out",
            );
        }
        assert!(
            setup
                .device()
                .join("Assets/gba/common/RPG/Two.gba")
                .exists()
        );
        assert!(setup.device().join("Saves/gba/common/RPG/Two.sav").exists());
    }

    #[test]
    fn warns_of_skipped_saves_and_exits_0_when_pulling_all() {
        let setup = two_copies();
        setup.on_device("Assets/gba/common/Here.gba", b"here");
        setup.on_device("Saves/gba/common/Here.sav", b"save");
        let ran = setup.run(&["pull", "save"]);
        assert_eq!(ran.code, 0);
        assert_eq!(ran.stdout, "gba/One.gba: no save on the device\n");
        assert_eq!(
            ran.stderr,
            format!(
                "{TWO_COPIES_WARNING}warning: these games on the device have saves but aren't in \
                 the library, so their saves weren't backed up: gba/Here.gba\n"
            )
        );
    }
}

/// Without `--at`, `trash save` trashes every version, so a version that
/// isn't there must stop it.
#[test]
fn exits_2_and_trashes_nothing_for_unknown_version() {
    let setup = Setup::new();
    setup.in_library("roms/gba/Mine.gba", b"mine");
    let kept = setup.in_library_at("saves/gba/Mine/a.sav", b"kept", 1_600_000_000);
    setup.on_device("Assets/gba/common/Mine.gba", b"mine");
    let reason = "gba/Mine.gba: no such version, as `pocketeer saves` shows them";
    assert_could_not_start(
        &setup.run(&["push", "save", "gba/Mine.gba", "--at", "never"]),
        &format!("error: {reason}"),
    );
    assert_json_could_not_start(
        &setup.run(&["--json", "trash", "save", "gba/Mine.gba", "--at", "never"]),
        reason,
    );
    assert!(kept.exists());
}

mod importing {
    use super::*;

    #[test]
    fn requires_platform_for_unknown_extension() {
        let setup = Setup::new();
        let file = setup.elsewhere("mine.bin", b"mine");
        let path = file.to_str().unwrap();
        let other = setup.elsewhere("other.gba", b"other");
        let other = other.to_str().unwrap();
        let ran = run_and_capture(setup.command().args(["import", "game", other, path]));
        assert_could_not_start(
            &ran,
            &format!("error: {path}: can't tell which platform it is for; give --platform"),
        );
        assert!(!setup.library().join("roms/gba/other.gba").exists());
        let ran = run_and_capture(setup.command().args([
            "import",
            "game",
            "--platform",
            "genesis",
            path,
        ]));
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert!(setup.library().join("roms/genesis/mine.bin").exists());
    }

    #[test]
    fn exits_2_for_unlisted_platform_id() {
        let setup = Setup::new();
        let file = setup.elsewhere("mine.bin", b"mine");
        let path = file.to_str().unwrap();
        let absolute = setup.dir.path().join("absolute");
        for platform in ["../../escaped", absolute.to_str().unwrap(), "arduboy"] {
            let args = ["--json", "import", "game", "--platform", platform, path];
            let ran = run_and_capture(setup.command().args(args));
            assert_eq!(ran.code, 2, "{}", ran.stderr);
            assert_eq!(ran.stdout, "");
            assert!(
                ran.stderr.contains("[possible values: gb, gbc, gba,"),
                "{}",
                ran.stderr
            );
        }
        assert!(!setup.dir.path().join("escaped").exists());
        assert!(!absolute.exists());
    }

    /// This stays a CLI test because only a child process can change its working
    /// folder without affecting the other tests running at the same time.
    #[test]
    fn logs_absolute_path_for_relative_path() {
        let setup = Setup::new();
        setup.in_library("roms/gba/Mine.gba", b"mine");
        let save = setup.elsewhere("Mine.sav", b"save");
        let folder = save.parent().unwrap();
        let game = put(folder, "mine.gba", b"game");
        let in_folder =
            |args: &[&str]| run_and_capture(setup.command().current_dir(folder).args(args));
        in_folder(&["import", "save", "gba/Mine.gba", "Mine.sav"]);
        in_folder(&["import", "game", "mine.gba"]);
        // Uses the real paths, because the temporary folder may be behind a
        // symlink.
        let logged = setup.log_lines();
        assert_eq!(logged[0]["from"], json!(fs::canonicalize(&save).unwrap()));
        assert_eq!(logged[1]["from"], json!(fs::canonicalize(&game).unwrap()));
    }
}

/// Lists orphan saves, refuses what isn't one, and deletes one from the
/// device.
#[test]
fn orphan_saves_listed_refused_and_deleted() {
    let setup = Setup::new();
    setup.on_device("Assets/gba/common/One.gba", b"one");
    let game_save = setup.on_device("Saves/gba/common/One.sav", b"save");
    let root = setup.device().join("Analogue_Pocket.json");
    let long = setup.on_device_at("Saves/gba/common/A.sav", &[0; 12], 1_600_000_000);
    setup.on_device_at("Saves/gba/common/B.sav", &[0; 3], 1_600_000_000);
    let in_library = setup.in_library_at("saves/gba/Gone/a.sav", b"orphan", 1_600_000_000);

    let ran = setup.run(&["orphan-saves", "device"]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    assert_eq!(
        ran.stdout,
        "SAVE           SIZE  WRITTEN\n\
         gba/A.sav  12 bytes  2020-09-13T12-26-40\n\
         gba/B.sav   3 bytes  2020-09-13T12-26-40\n"
    );
    assert_eq!(
        setup.run(&["--json", "orphan-saves", "device"]).json()["data"],
        json!([
            { "save": "gba/A.sav", "size": 12, "written": "2020-09-13T12-26-40" },
            { "save": "gba/B.sav", "size": 3, "written": "2020-09-13T12-26-40" },
        ])
    );
    assert_eq!(
        setup.run(&["--json", "orphan-saves", "library"]).json()["data"],
        json!([{ "save": "gba/Gone", "versions": 1, "newest": "a" }])
    );

    // A game's save, or a path out of the saves, isn't an orphan.
    for name in ["gba/One.sav", "gba/../../../Analogue_Pocket.json"] {
        let ran = setup.run(&["--json", "delete-on-device", "orphan-save", name]);
        assert_json_could_not_start(
            &ran,
            &format!("{name}: not an orphan save, as `pocketeer orphan-saves` shows them"),
        );
    }
    assert!(game_save.exists() && root.exists() && long.exists());
    assert_json_could_not_start(
        &setup.run(&["--json", "trash", "orphan-save", "gba/One"]),
        "gba/One: not an orphan save, as `pocketeer orphan-saves` shows them",
    );
    assert!(in_library.exists());

    let ran = setup.run(&["delete-on-device", "orphan-save", "gba/A.sav"]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    assert_eq!(ran.stdout, "gba/A.sav: save deleted from the device\n");
    assert!(!long.exists());
    assert_eq!(
        setup.run(&["--json", "orphan-saves", "device"]).json()["data"],
        json!([{ "save": "gba/B.sav", "size": 3, "written": "2020-09-13T12-26-40" }])
    );
}

#[test]
fn stops_batch_and_exits_1_when_log_unwritable() {
    let with_two_games_and_broken_log = || {
        let setup = Setup::new();
        setup.on_device("Assets/gba/common/One.gba", b"one");
        setup.on_device("Assets/gba/common/Two.gba", b"two");
        setup.break_the_log();
        setup
    };
    let setup = with_two_games_and_broken_log();
    let ran = setup.run(&["--json", "pull", "game"]);
    assert_eq!(ran.code, 1);
    let value = ran.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["data"].as_array().unwrap().len(), 1);
    assert!(ran.stderr.starts_with("error: couldn't write the log: "));
    let setup = with_two_games_and_broken_log();
    let ran = setup.run(&["pull", "game"]);
    assert_eq!(ran.code, 1);
    assert_eq!(ran.stdout, "gba/One.gba: pulled as gba/One.gba\n");
    assert!(
        ran.stderr.starts_with("error: couldn't write the log: "),
        "{}",
        ran.stderr
    );
    assert!(!setup.library().join("roms/gba/Two.gba").exists());
}

mod rename {
    use super::*;

    fn games_in_library(setup: &Setup) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(setup.library().join("roms/gba"))
            .unwrap()
            .map(|file| file.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// A library with the DAT and one game under a name of its own, which a
    /// command that adds games must leave alone.
    fn with_dat_and_old_game() -> Setup {
        let setup = Setup::new();
        // It also lists the contents `world` as `Puzzle (USA)`.
        let dat = format!(
            r#"{GBA_DAT}
game ( name "Puzzle (USA)" rom ( name "Puzzle (USA).gba" size 5 crc 3A771143 ) )"#
        );
        setup.in_library("dats/Nintendo - Game Boy Advance.dat", dat.as_bytes());
        setup.in_library("roms/gba/Old.gba", b"hello");
        setup
    }

    #[test]
    fn gives_standard_names_only_to_the_games_import_and_pull_add() {
        let setup = with_dat_and_old_game();
        let file = setup.elsewhere("puzzle.gba", b"world");
        let ran = setup.run(&["import", "game", "--standard-names", file.to_str().unwrap()]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            ran.stdout,
            "puzzle.gba: imported as gba/puzzle.gba\n\
             gba/puzzle.gba: renamed to gba/Puzzle (USA).gba\n"
        );
        assert_eq!(games_in_library(&setup), ["Old.gba", "Puzzle (USA).gba"]);

        let setup = with_dat_and_old_game();
        setup.on_device("Assets/gba/common/puzzle.gba", b"world");
        let ran = setup.run(&["pull", "game", "--standard-names"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(games_in_library(&setup), ["Old.gba", "Puzzle (USA).gba"]);
    }

    /// `rename standard` renames the games named, or every game the DATs know
    /// when none is named.
    #[test]
    fn rename_standard_renames_named_or_every_known_game() {
        let setup = with_dat_and_old_game();
        setup.in_library("roms/gba/Other.gba", b"world");
        setup.in_library("roms/gba/Mine.gba", b"unknown");

        let ran = setup.run(&["rename", "standard", "gba/Mine.gba"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.stdout, "no game to rename\n");
        let ran = setup.run(&["--json", "rename", "standard", "gba/Mine.gba"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.json()["data"], json!([]));

        let ran = setup.run(&["rename", "standard", "gba/Old.gba"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            games_in_library(&setup),
            ["Mine.gba", "Other.gba", "Tetris (World).gba"]
        );

        let ran = setup.run(&["rename", "standard"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            ran.stdout,
            "gba/Other.gba: renamed to gba/Puzzle (USA).gba\n"
        );
        assert_eq!(
            games_in_library(&setup),
            ["Mine.gba", "Puzzle (USA).gba", "Tetris (World).gba"]
        );
    }
}

/// Lists the files the cores load, refuses one no core loads, and puts one on
/// the device.
#[test]
fn bios_files_listed_and_put_on_device() {
    let setup = Setup::new();
    setup.on_device("Cores/a.GBA/core.json", core_json("1", "gba").as_bytes());
    let slot = r#"{"filename":"gba_bios.bin","required":true,"parameters":"0x88","size_exact":4}"#;
    setup.on_device("Cores/a.GBA/data.json", data_json(slot).as_bytes());
    assert_eq!(
        setup.run(&["--json", "bios", "list"]).json(),
        json!({
            "schema_version": 1,
            "ok": true,
            "data": [{
                "file": "gba/gba_bios.bin",
                "status": "missing",
                "size": null,
                "size_exact": 4,
                "required": true,
                "needed_by": [{"core": "a.GBA", "required": true}],
            }],
        })
    );

    let from = setup.elsewhere("mine.bin", b"four");
    let from = from.to_str().unwrap();
    assert_json_could_not_start(
        &setup.run(&["--json", "bios", "put", "gba/other.bin", from]),
        "gba/other.bin: no core on the device loads it, as `pocketeer bios list` shows",
    );
    let ran = setup.run(&["bios", "put", "gba/gba_bios.bin", from]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    assert_eq!(ran.stdout, "gba/gba_bios.bin: put on the device\n");
    let on_device = setup.device().join("Assets/gba/common/gba_bios.bin");
    assert_eq!(fs::read(&on_device).unwrap(), b"four");

    fs::write(&on_device, b"five!").unwrap();
    let ran = setup.run(&["bios", "list"]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    assert_eq!(
        ran.stdout,
        "FILE              STATUS                      NEEDED BY\n\
         gba/gba_bios.bin  wrong size: 5 bytes, not 4  a.GBA\n"
    );
    let entry = &setup.run(&["--json", "bios", "list"]).json()["data"][0];
    assert_eq!(entry["status"], "wrong_size");
    assert_eq!(entry["size"], 5);
}

mod cores {
    use super::*;

    const GB: &str = "Spiritualized.GB";

    /// Answers every request with the zip of `Spiritualized.GB` at `version`,
    /// so nothing goes online. Returns its address.
    fn serve_release(version: &str) -> String {
        let core_json_of_release = core_json(version, "gb");
        let body = zip_of(&[
            (
                "Cores/Spiritualized.GB/core.json",
                core_json_of_release.as_bytes(),
            ),
            ("Platforms/gb.json", b"{}"),
            ("README.md", b"readme"),
        ]);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let _ = stream.read(&mut [0; 4096]);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        format!("http://{address}/core.zip")
    }

    /// A library whose index, just written so that it isn't fetched again,
    /// has `Spiritualized.GB` at 2.0 at `url`.
    fn with_index(url: &str) -> Setup {
        let setup = Setup::new();
        setup.in_library("cores.json", &index_file(url));
        setup
    }

    fn core_on_device(setup: &Setup, id: &str, version: &str) {
        setup.on_device(
            &format!("Cores/{id}/core.json"),
            core_json(version, "gb").as_bytes(),
        );
    }

    /// Lists the cores, refuses what can't be installed or isn't on the
    /// device, updates the one with a newer release and removes it.
    #[test]
    fn cores_listed_updated_and_removed() {
        let setup = with_index(&serve_release("2.0"));
        core_on_device(&setup, GB, "1.0");
        core_on_device(&setup, "someone.Else", "3.0");
        assert_eq!(
            setup.run(&["--json", "cores", "list"]).json(),
            json!({
                "schema_version": 1,
                "ok": true,
                "data": [
                    {
                        "id": GB,
                        "platforms": ["gb"],
                        "installed": true,
                        "version": "1.0",
                        "latest": "2.0",
                        "update_available": true,
                    },
                    {
                        "id": "someone.Else",
                        "platforms": ["gb"],
                        "installed": true,
                        "version": "3.0",
                        "latest": null,
                        "update_available": false,
                    },
                ],
            })
        );
        let ran = setup.run(&["cores", "list"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            ran.stdout,
            "CORE              PLATFORM  ON THE DEVICE  LATEST\n\
             Spiritualized.GB  gb        1.0            2.0\n\
             someone.Else      gb        3.0            -\n"
        );

        let installed = setup.device().join("Cores").join(GB).join("core.json");
        assert_json_could_not_start(
            &setup.run(&["--json", "cores", "install", GB, "no.Such"]),
            "no.Such: not a core that can be installed, as `pocketeer cores list` shows",
        );
        assert_eq!(
            fs::read_to_string(&installed).unwrap(),
            core_json("1.0", "gb")
        );

        let ran = setup.run(&["cores", "update"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.stdout, "Spiritualized.GB: 2.0 installed\n");
        let ran = setup.run(&["cores", "update"]);
        assert_eq!(ran.stdout, "every core is up to date\n");

        // A path that leads out of the cores isn't a core.
        for what in ["update", "remove"] {
            assert_json_could_not_start(
                &setup.run(&["--json", "cores", what, "../Assets"]),
                "../Assets: not on the device",
            );
        }
        assert!(setup.device().join("Assets").exists());

        let ran = setup.run(&["cores", "remove", GB]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.stdout, "Spiritualized.GB: removed from the device\n");
        assert!(!setup.device().join("Cores").join(GB).exists());
    }

    #[test]
    fn exits_1_when_download_fails() {
        let setup = with_index("http://127.0.0.1:1/never");
        let ran = setup.run(&["cores", "install", GB]);
        assert_eq!(ran.code, 1);
        assert!(
            ran.stdout.starts_with("Spiritualized.GB: error: "),
            "{}",
            ran.stdout
        );
    }
}
