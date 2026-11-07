//! Tests the command line from the outside, covering exit codes, output streams
//! and the shape of `--json`. The core's tests cover what the commands do.
//!
//! Every command gets `--library` and `--device` or needs no device, so no test
//! touches a mounted Pocket. None puts anything in the system's trash.

use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, UNIX_EPOCH},
};

use serde_json::{Value, json};
use zip::{ZipWriter, write::SimpleFileOptions};

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
        setup.on_device("Analogue_Pocket.json", b"{}");
        fs::create_dir(setup.device().join("Cores")).unwrap();
        fs::create_dir(setup.device().join("Assets")).unwrap();
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

    /// A file apart from the library and the device.
    fn elsewhere(&self, name: &str, bytes: &[u8]) -> PathBuf {
        put(&self.dir.path().join("elsewhere"), name, bytes)
    }

    /// `pocketeer --library … --device … <args>`.
    fn run(&self, args: &[&str]) -> Ran {
        let mut command = pocketeer_command();
        command.arg("--library").arg(self.library());
        command.arg("--device").arg(self.device());
        run_and_capture(command.args(args))
    }

    fn log_lines(&self) -> Vec<Value> {
        fs::read_to_string(self.library().join("pocketeer.log"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
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

fn put(root: &Path, path: &str, bytes: &[u8]) -> PathBuf {
    let full = root.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(&full, bytes).unwrap();
    full
}

fn set_written_at(path: &Path, secs: u64) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
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

mod printing {
    use super::*;

    #[test]
    fn prints_games_as_table() {
        let setup = Setup::new();
        setup.in_library("roms/gba/Mine.gba", b"both");
        setup.on_device("Assets/gba/common/Both.gba", b"both");
        setup.on_device("Saves/gba/common/Both.sav", b"save");
        setup.on_device("Assets/gba/common/Here.gba", b"here");
        let ran = setup.run(&["games"]);
        assert_eq!(ran.code, 0);
        assert_eq!(
            ran.stdout,
            "LIBRARY       DEVICE        SAVE               PULLS AS\n\
             gba/Mine.gba  gba/Both.gba  new on the device\n\
             -             gba/Here.gba  -                  gba/Here.gba\n"
        );
    }

    #[test]
    fn prints_games_as_json() {
        let setup = Setup::new();
        setup.in_library("roms/gba/Mine.gba", b"both");
        let cover = setup.in_library("covers/gba/Mine.png", b"png");
        setup.in_library("saves/gba/Mine/a.sav", b"kept");
        setup.on_device("Assets/gba/common/Both.gba", b"both");
        setup.on_device("Assets/gba/common/Here.gba", b"here");
        let ran = setup.run(&["--json", "games"]);
        assert_eq!(ran.code, 0);
        assert_eq!(
            ran.json(),
            json!({
                "schema_version": 1,
                "ok": true,
                "data": [
                    {
                        "platform": "gba",
                        "library": "gba/Mine.gba",
                        "device": "gba/Both.gba",
                        "saves_kept": 1,
                        "new_save_on_device": false,
                        "pulls_as": null,
                        "cover": cover,
                    },
                    {
                        "platform": "gba",
                        "library": null,
                        "device": "gba/Here.gba",
                        "saves_kept": 0,
                        "new_save_on_device": false,
                        "pulls_as": "gba/Here.gba",
                        "cover": null,
                    },
                ],
                "duplicates": [],
            })
        );
    }

    #[test]
    fn prints_saves_as_json_newest_first() {
        let setup = Setup::new();
        setup.in_library("roms/gba/Mine.gba", b"game");
        let old = setup.in_library("saves/gba/Mine/old.sav", b"old");
        set_written_at(&old, 1_600_000_000);
        let new = setup.in_library("saves/gba/Mine/new.sav", b"newer");
        set_written_at(&new, 1_650_000_000);
        let ran = setup.run(&["--json", "saves", "gba/Mine.gba"]);
        assert_eq!(
            ran.json()["data"],
            json!([
                { "version": "new", "size": 5, "newest": true },
                { "version": "old", "size": 3, "newest": false },
            ])
        );
    }

    #[test]
    fn prints_orphan_saves_as_json() {
        let setup = Setup::new();
        let on_device = setup.on_device("Saves/gba/common/RPG/Gone.sav", b"save");
        set_written_at(&on_device, 1_600_000_000);
        let in_library = setup.in_library("saves/gba/Gone/a.sav", b"save");
        set_written_at(&in_library, 1_600_000_000);
        assert_eq!(
            setup.run(&["--json", "orphan-saves", "device"]).json()["data"],
            json!([{
                "save": "gba/RPG/Gone.sav",
                "size": 4,
                "written": "2020-09-13T12-26-40",
            }])
        );
        assert_eq!(
            setup.run(&["--json", "orphan-saves", "library"]).json()["data"],
            json!([{
                "save": "gba/Gone",
                "versions": 1,
                "newest": "a",
            }])
        );
    }

    #[test]
    fn aligns_sizes_to_the_right() {
        let setup = Setup::new();
        let long = setup.on_device("Saves/gba/common/A.sav", &[0; 12]);
        let short = setup.on_device("Saves/gba/common/B.sav", &[0; 3]);
        set_written_at(&long, 1_600_000_000);
        set_written_at(&short, 1_600_000_000);
        let ran = setup.run(&["orphan-saves", "device"]);
        assert_eq!(
            ran.stdout,
            "SAVE           SIZE  WRITTEN\n\
         gba/A.sav  12 bytes  2020-09-13T12-26-40\n\
         gba/B.sav   3 bytes  2020-09-13T12-26-40\n"
        );
    }
}

mod batches {
    use super::*;

    #[test]
    fn exits_0_and_reports_each_game() {
        let setup = Setup::new();
        setup.on_device("Assets/gba/common/One.gba", b"one");
        setup.on_device("Assets/gba/common/Two.gba", b"two");
        let ran = setup.run(&["pull", "game"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            ran.stdout,
            "gba/One.gba: pulled as gba/One.gba\ngba/Two.gba: pulled as gba/Two.gba\n"
        );
        assert_eq!(ran.stderr, "");
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
}

mod status {
    use super::*;

    /// A device with a firmware, a core and a game.
    fn with_firmware() -> Setup {
        let setup = Setup::new();
        setup.on_device(
            "Analogue_Pocket.json",
            br#"{"firmware": {"runtime": {"name": "2.6", "build_date": "2026-06-16"}}}"#,
        );
        setup.on_device("Cores/a.B/core.json", b"{}");
        setup.on_device("Assets/gba/common/Mine.gba", b"game");
        setup
    }

    #[test]
    fn prints_status_as_text() {
        let setup = with_firmware();
        let ran = setup.run(&["status"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        let lines: Vec<&str> = ran.stdout.lines().collect();
        assert_eq!(lines[0], format!("root      {}", setup.device().display()));
        assert_eq!(lines[1], "firmware  2.6 (built 2026-06-16)");
        assert!(lines[2].starts_with("space     "), "{}", lines[2]);
        assert_eq!(lines[3], "cores     1");
        assert_eq!(lines[4], "games     1");
    }

    #[test]
    fn prints_status_as_json() {
        let setup = with_firmware();
        let value = setup.run(&["--json", "status"]).json();
        assert_eq!(value["ok"], true);
        let data = &value["data"];
        assert_eq!(data["root"], setup.device().display().to_string());
        assert_eq!(
            data["firmware"],
            serde_json::json!({ "version": "2.6", "build_date": "2026-06-16" })
        );
        assert!(data["total_bytes"].as_u64().unwrap() > 0);
        assert!(data["available_bytes"].is_u64());
        assert_eq!(data["cores"], 1);
        assert_eq!(data["games"], 1);
    }

    #[test]
    fn prints_null_firmware_when_unknown() {
        let setup = Setup::new();
        let value = setup.run(&["--json", "status"]).json();
        assert_eq!(value["data"]["firmware"], Value::Null);
    }
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

    #[test]
    fn exits_2_for_game_without_platform_prefix() {
        let setup = Setup::new();
        let how = "write a game as <platform>/<name>, as `pocketeer games` shows it";
        assert_could_not_start(
            &setup.run(&["push", "game", "Mine.gba"]),
            &format!("error: Mine.gba: {how}"),
        );
        assert_json_could_not_start(
            &setup.run(&["--json", "push", "game", "Mine.gba"]),
            &format!("Mine.gba: {how}"),
        );
    }

    #[test]
    fn exits_2_and_does_nothing_for_game_not_found() {
        let setup = Setup::new();
        setup.on_device("Assets/gba/common/One.gba", b"one");
        assert_could_not_start(
            &setup.run(&["pull", "game", "gba/One.gba", "gba/Nope.gba"]),
            "error: gba/Nope.gba: not on the device",
        );
        assert!(!setup.library().join("roms/gba/One.gba").exists());
        assert_json_could_not_start(
            &setup.run(&["--json", "pull", "game", "gba/Nope.gba"]),
            "gba/Nope.gba: not on the device",
        );
        assert_json_could_not_start(
            &setup.run(&["--json", "push", "game", "gba/Nope.gba"]),
            "gba/Nope.gba: not in the library",
        );
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
            pocketeer_command()
                .arg("--json")
                .arg("--library")
                .arg(setup.library())
                .arg("--device")
                .arg(setup.library())
                .arg("games"),
        );
        let reason = format!(
            "{} is not a Pocket: no Analogue_Pocket.json, Assets, Cores",
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

    const TWO_COPIES_WARNING: &str = "warning: gba/RPG/Two.gba is the same game as gba/One.gba; \
                                      it is left out and its save isn't synced\n";

    #[test]
    fn warns_of_duplicates_and_lists_them_in_json() {
        let setup = two_copies();
        let ran = setup.run(&["--json", "games"]);
        assert_eq!(ran.code, 0);
        assert_eq!(ran.stderr, TWO_COPIES_WARNING);
        assert_eq!(
            ran.json()["duplicates"],
            json!([{ "device": "gba/RPG/Two.gba", "same_game_as": "gba/One.gba" }])
        );
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
                "gba/RPG/Two.gba: the same game as gba/One.gba, the copy that is used",
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

mod versions {
    use super::*;

    /// A game on both sides, with a save kept in the library at a known time.
    fn with_a_version() -> Setup {
        let setup = Setup::new();
        setup.in_library("roms/gba/Mine.gba", b"mine");
        let kept = setup.in_library("saves/gba/Mine/a.sav", b"kept");
        set_written_at(&kept, 1_600_000_000);
        setup.on_device("Assets/gba/common/Mine.gba", b"mine");
        setup
    }

    #[test]
    fn exits_2_without_version_or_with_unknown_one() {
        let setup = with_a_version();
        let ran = setup.run(&["push", "save", "gba/Mine.gba"]);
        assert_eq!(ran.code, 2);
        assert_eq!(ran.stdout, "");
        let ran = setup.run(&["push", "save", "gba/Mine.gba", "--at", "never"]);
        assert_eq!(ran.code, 2);
        assert_eq!(
            ran.stderr,
            "error: gba/Mine.gba: no such version, as `pocketeer saves` shows them\n"
        );
        let ran = setup.run(&["--json", "trash", "save", "gba/Mine.gba", "--at", "never"]);
        assert_eq!(ran.code, 2);
        assert_eq!(ran.json()["ok"], false);
        assert!(setup.library().join("saves/gba/Mine/a.sav").exists());
    }

    #[test]
    fn pushes_version_by_name() {
        let setup = with_a_version();
        let ran = setup.run(&["push", "save", "gba/Mine.gba", "--at", "a"]);
        assert_eq!(ran.code, 0);
        assert_eq!(ran.stdout, "gba/Mine.gba: save a pushed\n");
        assert_eq!(
            fs::read(setup.device().join("Saves/gba/common/Mine.sav")).unwrap(),
            b"kept"
        );
    }

    #[test]
    fn names_versions_of_same_second_apart_and_takes_either() {
        let setup = Setup::new();
        setup.in_library("roms/gba/Mine.gba", b"mine");
        setup.on_device("Assets/gba/common/Mine.gba", b"mine");
        let first = setup.elsewhere("first.sav", b"first");
        let second = setup.elsewhere("second.sav", b"second");
        set_written_at(&first, 1_600_000_000);
        set_written_at(&second, 1_600_000_000);
        for file in [&first, &second] {
            let imported = setup.run(&["import", "save", "gba/Mine.gba", &file.to_string_lossy()]);
            assert_eq!(imported.code, 0);
        }
        let ran = setup.run(&["saves", "gba/Mine.gba"]);
        assert_eq!(
            ran.stdout,
            "VERSION                   SIZE\n\
             2020-09-13T12-26-40-2  6 bytes  newest\n\
             2020-09-13T12-26-40    5 bytes\n"
        );
        for (name, bytes) in [
            ("2020-09-13T12-26-40-2", "second"),
            ("2020-09-13T12-26-40", "first"),
        ] {
            let ran = setup.run(&["push", "save", "gba/Mine.gba", "--at", name]);
            assert_eq!(ran.code, 0);
            assert_eq!(ran.stdout, format!("gba/Mine.gba: save {name} pushed\n"));
            let on_device = setup.device().join("Saves/gba/common/Mine.sav");
            assert_eq!(fs::read(on_device).unwrap(), bytes.as_bytes());
        }
    }
}

mod importing {
    use super::*;

    /// The start of a PNG file, enough to be recognized as one.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

    /// A game in the library, no device, and a save in a folder apart from them.
    fn with_a_save_to_import() -> (Setup, PathBuf) {
        let setup = Setup::new();
        setup.in_library("roms/gba/Mine.gba", b"mine");
        let file = setup.elsewhere("Mine.sav", b"save");
        set_written_at(&file, 1_600_000_000);
        (setup, file)
    }

    /// `pocketeer --library … <args>`, for importing, which needs no device.
    fn without_a_device(setup: &Setup, args: &[&str]) -> Ran {
        run_and_capture(
            pocketeer_command()
                .arg("--library")
                .arg(setup.library())
                .args(args),
        )
    }

    #[test]
    fn prints_imported_save_as_json() {
        let (setup, file) = with_a_save_to_import();
        let file = file.to_str().unwrap();
        let ran = without_a_device(&setup, &["--json", "import", "save", "gba/Mine.gba", file]);
        assert_eq!(
            ran.json(),
            json!({
                "schema_version": 1,
                "ok": true,
                "data": [{
                    "front_end": "cli",
                    "game": "gba/Mine.gba",
                    "ok": true,
                    "message": "save 2020-09-13T12-26-40 imported",
                    "from": file,
                    "version": "2020-09-13T12-26-40",
                }],
            })
        );
    }

    #[test]
    fn prints_imported_cover_as_json() {
        let setup = Setup::new();
        setup.in_library("roms/gba/Mine.gba", b"mine");
        let file = setup.elsewhere("mine.png", PNG);
        let path = file.to_str().unwrap();
        let ran = without_a_device(&setup, &["--json", "cover", "import", "gba/Mine.gba", path]);
        assert_eq!(
            ran.json(),
            json!({
                "schema_version": 1,
                "ok": true,
                "data": [{
                    "front_end": "cli",
                    "game": "gba/Mine.gba",
                    "ok": true,
                    "message": "cover imported",
                    "from": path,
                }],
            })
        );
    }

    #[test]
    fn imports_games_under_dir() {
        let setup = Setup::new();
        setup.elsewhere("roms/old/Deep.gb", b"deep");
        setup.elsewhere("roms/README.txt", b"read me");
        let folder = setup.elsewhere("roms/Mine.gba", b"mine");
        let folder = folder.parent().unwrap().to_str().unwrap();
        let ran = without_a_device(&setup, &["import", "game", folder]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            ran.stdout,
            "Mine.gba: imported as gba/Mine.gba\nDeep.gb: imported as gb/Deep.gb\n"
        );
    }

    #[test]
    fn prints_imported_game_as_json() {
        let setup = Setup::new();
        let file = setup.elsewhere("mine.gba", b"mine");
        let path = file.to_str().unwrap();
        let ran = without_a_device(&setup, &["--json", "import", "game", path]);
        assert_eq!(
            ran.json(),
            json!({
                "schema_version": 1,
                "ok": true,
                "data": [{
                    "front_end": "cli",
                    "game": "mine.gba",
                    "ok": true,
                    "message": "imported as gba/mine.gba",
                    "from": path,
                    "library": "gba/mine.gba",
                }],
            })
        );
    }

    #[test]
    fn requires_platform_for_unknown_extension() {
        let setup = Setup::new();
        let file = setup.elsewhere("mine.bin", b"mine");
        let path = file.to_str().unwrap();
        let ran = without_a_device(&setup, &["import", "game", path]);
        assert_eq!(ran.code, 1);
        assert_eq!(
            ran.stdout,
            "mine.bin: can't tell which platform it is for\n"
        );
        let ran = without_a_device(&setup, &["import", "game", "--platform", "genesis", path]);
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
            assert_json_could_not_start(
                &without_a_device(&setup, &args),
                &format!(
                    "{platform}: not a platform; these are: gb, gbc, gba, gg, sms, nes, snes, genesis, pce, ng"
                ),
            );
        }
        assert!(!setup.dir.path().join("escaped").exists());
        assert!(!absolute.exists());
    }

    #[test]
    fn logs_absolute_path_for_relative_path() {
        let (setup, file) = with_a_save_to_import();
        let folder = file.parent().unwrap();
        let game = put(folder, "mine.gba", b"game");
        let in_folder = |args: &[&str]| {
            run_and_capture(
                pocketeer_command()
                    .current_dir(folder)
                    .arg("--library")
                    .arg(setup.library())
                    .args(args),
            )
        };
        in_folder(&["import", "save", "gba/Mine.gba", "Mine.sav"]);
        in_folder(&["import", "game", "mine.gba"]);
        // Uses the real paths, because the temporary folder may be behind a
        // symlink.
        let logged = setup.log_lines();
        assert_eq!(logged[0]["from"], json!(fs::canonicalize(&file).unwrap()));
        assert_eq!(logged[1]["from"], json!(fs::canonicalize(&game).unwrap()));
    }
}

mod deleting {
    use super::*;

    #[test]
    fn deletes_orphan_save() {
        let setup = Setup::new();
        let save = setup.on_device("Saves/gba/common/One.sav", b"save");
        let ran = setup.run(&["delete-on-device", "orphan-save", "gba/One.sav"]);
        assert_eq!(ran.code, 0);
        assert_eq!(ran.stdout, "gba/One.sav: save deleted from the device\n");
        assert!(!save.exists());
    }

    #[test]
    fn exits_2_for_save_that_is_not_orphan() {
        let setup = Setup::new();
        setup.on_device("Assets/gba/common/One.gba", b"one");
        let save = setup.on_device("Saves/gba/common/One.sav", b"save");
        let root = setup.on_device("Analogue_Pocket.json", b"{}");
        let orphan = setup.on_device("Saves/gba/common/Gone.sav", b"orphan");
        let in_library = setup.in_library("saves/gba/Gone/a.sav", b"orphan");
        for name in ["gba/One.sav", "gba/../../../Analogue_Pocket.json"] {
            let ran = setup.run(&["--json", "delete-on-device", "orphan-save", name]);
            assert_json_could_not_start(
                &ran,
                &format!("{name}: not an orphan save, as `orphan-saves` shows them"),
            );
        }
        assert!(save.exists() && root.exists() && orphan.exists());
        assert_json_could_not_start(
            &setup.run(&["--json", "trash", "orphan-save", "gba/One"]),
            "gba/One: not an orphan save, as `orphan-saves` shows them",
        );
        assert!(in_library.exists());
    }
}

mod log {
    use super::*;

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
}

mod rename {
    use super::*;

    /// A DAT that lists the contents `hello` as `Tetris (World)` and `world` as
    /// `Puzzle (USA)`.
    const GBA_DAT: &str = r#"clrmamepro ( name "Nintendo - Game Boy Advance" )
game ( name "Tetris (World)" rom ( name "Tetris (World).gba" size 5 crc 3610A686 ) )
game ( name "Puzzle (USA)" rom ( name "Puzzle (USA).gba" size 5 crc 3A771143 ) )"#;

    /// A library with the DAT, and three games under names of their own, the
    /// last of them not in the DAT.
    fn with_dat_and_games() -> Setup {
        let setup = Setup::new();
        setup.in_library("dats/Nintendo - Game Boy Advance.dat", GBA_DAT.as_bytes());
        setup.in_library("roms/gba/Old.gba", b"hello");
        setup.in_library("roms/gba/Other.gba", b"world");
        setup.in_library("roms/gba/Mine.gba", b"unknown");
        setup
    }

    fn games_in_library(setup: &Setup) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(setup.library().join("roms/gba"))
            .unwrap()
            .map(|file| file.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn renames_every_game_dats_know_without_games_given() {
        let setup = with_dat_and_games();
        let ran = setup.run(&["rename", "standard"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            ran.stdout,
            "gba/Old.gba: renamed to gba/Tetris (World).gba\n\
             gba/Other.gba: renamed to gba/Puzzle (USA).gba\n"
        );
        assert_eq!(
            games_in_library(&setup),
            ["Mine.gba", "Puzzle (USA).gba", "Tetris (World).gba"]
        );
    }

    #[test]
    fn renames_only_games_given() {
        let setup = with_dat_and_games();
        let ran = setup.run(&["rename", "standard", "gba/Old.gba"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            games_in_library(&setup),
            ["Mine.gba", "Other.gba", "Tetris (World).gba"]
        );
    }

    #[test]
    fn says_no_game_to_rename_for_game_dats_do_not_know() {
        let setup = with_dat_and_games();
        let ran = setup.run(&["rename", "standard", "gba/Mine.gba"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.stdout, "no game to rename\n");
        let ran = setup.run(&["--json", "rename", "standard", "gba/Mine.gba"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.json()["data"], json!([]));
    }

    #[test]
    fn exits_2_and_renames_nothing_for_game_not_in_library() {
        let setup = with_dat_and_games();
        assert_could_not_start(
            &setup.run(&["rename", "standard", "gba/Old.gba", "gba/Nope.gba"]),
            "error: gba/Nope.gba: not in the library",
        );
        assert_eq!(
            games_in_library(&setup),
            ["Mine.gba", "Old.gba", "Other.gba"]
        );
    }

    #[test]
    fn renames_game_on_device_with_its_save() {
        let setup = Setup::new();
        setup.on_device("Assets/gba/common/RPG/Old.gba", b"hello");
        setup.on_device("Saves/gba/common/RPG/Old.sav", b"save");
        let ran = setup.run(&["rename", "device", "gba/RPG/Old.gba", "New"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.stdout, "gba/RPG/Old.gba: renamed to gba/RPG/New.gba\n");
        assert!(setup.device().join("Saves/gba/common/RPG/New.sav").exists());
    }

    #[test]
    fn prints_library_rename_as_json() {
        let setup = Setup::new();
        setup.in_library("roms/gba/Old.gba", b"hello");
        setup.in_library("saves/gba/Old/x.sav", b"save");
        let ran = setup.run(&["--json", "rename", "library", "gba/Old.gba", "New"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            ran.json(),
            json!({
                "schema_version": 1,
                "ok": true,
                "data": [{
                    "front_end": "cli",
                    "game": "gba/Old.gba",
                    "ok": true,
                    "message": "renamed to gba/New.gba",
                    "library": "gba/New.gba",
                }],
            })
        );
        assert!(setup.library().join("saves/gba/New/x.sav").exists());
    }
}

mod bios {
    use super::*;

    /// A device with a core that loads `gba_bios.bin` of 4 bytes.
    fn with_core() -> Setup {
        let setup = Setup::new();
        setup.on_device(
            "Cores/a.GBA/core.json",
            br#"{"core":{"metadata":{"version":"1","platform_ids":["gba"]}}}"#,
        );
        setup.on_device(
            "Cores/a.GBA/data.json",
            br#"{"data":{"data_slots":[{"filename":"gba_bios.bin","required":true,"parameters":"0x88","size_exact":4}]}}"#,
        );
        setup
    }

    #[test]
    fn prints_list_as_json() {
        let setup = with_core();
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
    }

    #[test]
    fn prints_wrong_size_in_list_table() {
        let setup = with_core();
        setup.on_device("Assets/gba/common/gba_bios.bin", b"five!");
        let ran = setup.run(&["bios", "list"]);
        assert_eq!(ran.code, 0);
        assert_eq!(
            ran.stdout,
            "FILE              STATUS                      NEEDED BY\n\
             gba/gba_bios.bin  wrong size: 5 bytes, not 4  a.GBA\n"
        );
    }

    #[test]
    fn puts_file_on_device() {
        let setup = with_core();
        let from = setup.elsewhere("mine.bin", b"four");
        let ran = setup.run(&["bios", "put", "gba/gba_bios.bin", from.to_str().unwrap()]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.stdout, "gba/gba_bios.bin: put on the device\n");
        let on_device = setup.device().join("Assets/gba/common/gba_bios.bin");
        assert_eq!(fs::read(on_device).unwrap(), b"four");
    }

    #[test]
    fn exits_2_for_file_no_core_loads() {
        let setup = with_core();
        let from = setup.elsewhere("mine.bin", b"four");
        let from = from.to_str().unwrap();
        let ran = setup.run(&["--json", "bios", "put", "gba/other.bin", from]);
        assert_json_could_not_start(
            &ran,
            "gba/other.bin: no core on the device loads it, as `bios list` shows",
        );
    }
}

mod cores {
    use super::*;

    const GB: &str = "Spiritualized.GB";

    /// Answers every request with the zip of `Spiritualized.GB` at `version`,
    /// so nothing goes online. Returns its address.
    fn serve_release(version: &str) -> String {
        let core_json =
            format!(r#"{{"core":{{"metadata":{{"version":"{version}","platform_ids":["gb"]}}}}}}"#);
        let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes) in [
            ("Cores/Spiritualized.GB/core.json", core_json.as_bytes()),
            ("Platforms/gb.json", b"{}"),
            ("README.md", b"readme"),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let body = zip.finish().unwrap().into_inner();
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

    /// A library whose inventory, just written so that it isn't fetched again,
    /// has `Spiritualized.GB` at 2.0 at `url`.
    fn with_inventory(url: &str) -> Setup {
        let setup = Setup::new();
        let inventory = json!([{
            "id": GB,
            "version": "2.0",
            "platforms": ["gb"],
            "download_url": url,
        }]);
        setup.in_library("cores.json", inventory.to_string().as_bytes());
        setup
    }

    fn core_on_device(setup: &Setup, id: &str, version: &str) {
        let json =
            format!(r#"{{"core":{{"metadata":{{"version":"{version}","platform_ids":["gb"]}}}}}}"#);
        setup.on_device(&format!("Cores/{id}/core.json"), json.as_bytes());
    }

    #[test]
    fn prints_list_as_json() {
        let setup = with_inventory("http://127.0.0.1:1/never");
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
    }

    #[test]
    fn prints_list_as_table() {
        let setup = with_inventory("http://127.0.0.1:1/never");
        core_on_device(&setup, GB, "1.0");
        let ran = setup.run(&["cores", "list"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            ran.stdout,
            "CORE              PLATFORM  ON THE DEVICE  LATEST\n\
             Spiritualized.GB  gb        1.0            2.0\n"
        );
    }

    #[test]
    fn prints_install_as_json_with_entry_for_each_core() {
        let setup = with_inventory(&serve_release("2.0"));
        assert_eq!(
            setup.run(&["--json", "cores", "install", GB]).json(),
            json!({
                "schema_version": 1,
                "ok": true,
                "data": [{
                    "front_end": "cli",
                    "game": GB,
                    "ok": true,
                    "message": "2.0 installed",
                    "version": "2.0",
                }],
            })
        );
        assert!(
            setup
                .device()
                .join("Cores")
                .join(GB)
                .join("core.json")
                .exists()
        );
    }

    #[test]
    fn exits_2_for_unsupported_core_and_installs_nothing() {
        let setup = with_inventory(&serve_release("2.0"));
        assert_json_could_not_start(
            &setup.run(&["--json", "cores", "install", GB, "no.Such"]),
            "no.Such: not a core that can be installed, as `cores list` shows",
        );
        assert!(!setup.device().join("Cores").join(GB).exists());
    }

    #[test]
    fn exits_1_when_download_fails() {
        let setup = with_inventory("http://127.0.0.1:1/never");
        let ran = setup.run(&["cores", "install", GB]);
        assert_eq!(ran.code, 1);
        assert!(
            ran.stdout.starts_with("Spiritualized.GB: error: "),
            "{}",
            ran.stdout
        );
    }

    #[test]
    fn updates_cores_with_newer_release_when_none_named() {
        let setup = with_inventory(&serve_release("2.0"));
        core_on_device(&setup, GB, "1.0");
        core_on_device(&setup, "someone.Else", "3.0");
        let ran = setup.run(&["cores", "update"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.stdout, "Spiritualized.GB: 2.0 installed\n");
        let ran = setup.run(&["cores", "update"]);
        assert_eq!(ran.stdout, "every core is up to date\n");
    }

    #[test]
    fn exits_2_for_update_or_remove_of_core_not_on_device() {
        let setup = with_inventory("http://127.0.0.1:1/never");
        for what in ["update", "remove"] {
            assert_json_could_not_start(
                &setup.run(&["--json", "cores", what, "../Assets"]),
                "../Assets: not on the device",
            );
        }
        assert!(setup.device().join("Assets").exists());
    }

    #[test]
    fn removes_core_from_device() {
        let setup = with_inventory("http://127.0.0.1:1/never");
        core_on_device(&setup, GB, "1.0");
        let ran = setup.run(&["cores", "remove", GB]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.stdout, "Spiritualized.GB: removed from the device\n");
        assert!(!setup.device().join("Cores").join(GB).exists());
    }
}
