//! The command line from the outside: exit codes, what goes to which stream,
//! and the shape of `--json`. What the commands do is tested in the core.
//!
//! Every command is given `--library` and `--device`, or needs no device, so
//! that no test touches a Pocket that happens to be mounted. None puts anything
//! in the system's trash.

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

    /// Makes the log impossible to write: a folder is where it goes.
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

/// A command that couldn't start: nothing on stdout as text, the reason on
/// stderr, and exit code 2.
#[track_caller]
fn assert_could_not_start(ran: &Ran, stderr: &str) {
    assert_eq!(ran.code, 2, "{}", ran.stderr);
    assert_eq!(ran.stdout, "");
    assert_eq!(ran.stderr, format!("{stderr}\n"));
}

/// The same as JSON: the reason on stdout, with its code.
#[track_caller]
fn assert_json_could_not_start(ran: &Ran, code: &str) {
    assert_eq!(ran.code, 2, "{}", ran.stderr);
    let value = ran.json();
    assert_eq!(value["ok"], false);
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["error"]["code"], code);
    assert!(value["error"]["message"].is_string());
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
        let s = Setup::new();
        s.in_library("roms/gba/Mine.gba", b"both");
        s.on_device("Assets/gba/common/Both.gba", b"both");
        s.on_device("Saves/gba/common/Both.sav", b"save");
        s.on_device("Assets/gba/common/Here.gba", b"here");
        let ran = s.run(&["games"]);
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
        let s = Setup::new();
        s.in_library("roms/gba/Mine.gba", b"both");
        let cover = s.in_library("covers/gba/Mine.png", b"png");
        s.in_library("saves/gba/Mine/a.sav", b"kept");
        s.on_device("Assets/gba/common/Both.gba", b"both");
        s.on_device("Assets/gba/common/Here.gba", b"here");
        let ran = s.run(&["--json", "games"]);
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
    fn prints_saves_newest_first() {
        let s = Setup::new();
        s.in_library("roms/gba/Mine.gba", b"game");
        let old = s.in_library("saves/gba/Mine/old.sav", b"old");
        set_written_at(&old, 1_600_000_000);
        let new = s.in_library("saves/gba/Mine/new.sav", b"newer");
        set_written_at(&new, 1_650_000_000);
        let ran = s.run(&["--json", "saves", "gba/Mine.gba"]);
        assert_eq!(
            ran.json()["data"],
            json!([
                { "version": "2022-04-15T05-20-00", "size": 5, "newest": true },
                { "version": "2020-09-13T12-26-40", "size": 3, "newest": false },
            ])
        );
    }

    #[test]
    fn prints_orphan_saves_as_json() {
        let s = Setup::new();
        let on_device = s.on_device("Saves/gba/common/RPG/Gone.sav", b"save");
        set_written_at(&on_device, 1_600_000_000);
        let in_library = s.in_library("saves/gba/Gone/a.sav", b"save");
        set_written_at(&in_library, 1_600_000_000);
        assert_eq!(
            s.run(&["--json", "orphan-saves", "device"]).json()["data"],
            json!([{
                "save": "gba/RPG/Gone.sav",
                "size": 4,
                "written": "2020-09-13T12-26-40",
            }])
        );
        assert_eq!(
            s.run(&["--json", "orphan-saves", "library"]).json()["data"],
            json!([{
                "save": "gba/Gone",
                "versions": 1,
                "newest": "2020-09-13T12-26-40",
            }])
        );
    }
}

mod batches {
    use super::*;

    #[test]
    fn exits_0_and_reports_each_game() {
        let s = Setup::new();
        s.on_device("Assets/gba/common/One.gba", b"one");
        s.on_device("Assets/gba/common/Two.gba", b"two");
        let ran = s.run(&["pull", "game"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            ran.stdout,
            "gba/One.gba: pulled as gba/One.gba\ngba/Two.gba: pulled as gba/Two.gba\n"
        );
        assert_eq!(ran.stderr, "");
    }

    #[test]
    fn continues_after_failure_and_exits_1() {
        let s = Setup::new();
        s.in_library("roms/gba/None.gba", b"none");
        s.in_library("roms/gba/Mine.gba", b"mine");
        s.in_library("saves/gba/Mine/a.sav", b"kept");
        s.on_device("Assets/gba/common/None.gba", b"none");
        s.on_device("Assets/gba/common/Mine.gba", b"mine");
        let ran = s.run(&["--json", "push", "save", "gba/None.gba", "gba/Mine.gba"]);
        assert_eq!(ran.code, 1);
        let value = ran.json();
        assert_eq!(value["ok"], false);
        let ok: Vec<&Value> = value["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| &e["ok"])
            .collect();
        assert_eq!(ok, [&json!(false), &json!(true)]);
    }

    #[test]
    fn exits_0_when_nothing_to_do() {
        let s = Setup::new();
        s.in_library("roms/gba/None.gba", b"none");
        s.on_device("Assets/gba/common/None.gba", b"none");
        let ran = s.run(&["pull", "save", "gba/None.gba"]);
        assert_eq!(ran.code, 0);
        assert_eq!(ran.stdout, "gba/None.gba: no save on the device\n");
    }
}

mod could_not_start {
    use super::*;

    #[test]
    fn exits_2_with_empty_stdout_for_unknown_command() {
        let s = Setup::new();
        let ran = s.run(&["--json", "no-such-command"]);
        assert_eq!(ran.code, 2);
        assert_eq!(ran.stdout, "");
        assert!(ran.stderr.contains("no-such-command"), "{}", ran.stderr);
    }

    #[test]
    fn exits_2_without_command_outside_terminal() {
        let s = Setup::new();
        let ran = s.run(&[]);
        assert_could_not_start(
            &ran,
            "error: the terminal UI needs a terminal; give a command, or see --help",
        );
    }

    #[test]
    fn exits_2_for_game_without_platform_prefix() {
        let s = Setup::new();
        let how = "write a game as <platform>/<name>, as `pocketeer games` shows it";
        assert_could_not_start(
            &s.run(&["push", "game", "Mine.gba"]),
            &format!("error: Mine.gba: {how}"),
        );
        assert_json_could_not_start(&s.run(&["--json", "push", "game", "Mine.gba"]), "bad_game");
    }

    #[test]
    fn exits_2_and_does_nothing_for_game_not_found() {
        let s = Setup::new();
        s.on_device("Assets/gba/common/One.gba", b"one");
        assert_could_not_start(
            &s.run(&["pull", "game", "gba/One.gba", "gba/Nope.gba"]),
            "error: gba/Nope.gba: not on the device",
        );
        assert!(!s.library().join("roms/gba/One.gba").exists());
        assert_json_could_not_start(
            &s.run(&["--json", "pull", "game", "gba/Nope.gba"]),
            "not_on_device",
        );
        assert_json_could_not_start(
            &s.run(&["--json", "push", "game", "gba/Nope.gba"]),
            "not_in_library",
        );
    }

    #[test]
    fn exits_2_without_creating_missing_library() {
        let s = Setup::new();
        let typo = s.dir.path().join("typo");
        let ran = run_and_capture(
            pocketeer_command()
                .arg("--json")
                .arg("--library")
                .arg(&typo)
                .args(["orphan-saves", "library"]),
        );
        assert_json_could_not_start(&ran, "no_library");
        assert!(!typo.exists());
    }

    #[test]
    fn exits_2_for_dir_that_is_not_device() {
        let s = Setup::new();
        let ran = run_and_capture(
            pocketeer_command()
                .arg("--json")
                .arg("--library")
                .arg(s.library())
                .arg("--device")
                .arg(s.library())
                .arg("games"),
        );
        assert_json_could_not_start(&ran, "not_a_device");
    }

    #[test]
    fn exits_2_for_library_with_duplicate_game() {
        let s = Setup::new();
        s.in_library("roms/gba/One.gba", b"same");
        s.in_library("roms/gba/Two.gba", b"same");
        assert_json_could_not_start(&s.run(&["--json", "games"]), "same_game_in_library");
    }
}

mod duplicates_on_device {
    use super::*;

    /// One game with two copies; only the later has a save.
    fn two_copies() -> Setup {
        let s = Setup::new();
        s.in_library("roms/gba/Mine.gba", b"copy");
        s.on_device("Assets/gba/common/One.gba", b"copy");
        s.on_device("Assets/gba/common/RPG/Two.gba", b"copy");
        s.on_device("Saves/gba/common/RPG/Two.sav", b"save");
        s
    }

    const TWO_COPIES_WARNING: &str = "warning: gba/RPG/Two.gba is the same game as gba/One.gba; \
                                      it is left out and its save isn't synced\n";

    #[test]
    fn warns_of_duplicates_and_lists_them_in_json() {
        let s = two_copies();
        let ran = s.run(&["--json", "games"]);
        assert_eq!(ran.code, 0);
        assert_eq!(ran.stderr, TWO_COPIES_WARNING);
        assert_eq!(
            ran.json()["duplicates"],
            json!([{ "device": "gba/RPG/Two.gba", "same_game_as": "gba/One.gba" }])
        );
    }

    #[test]
    fn exits_2_for_command_naming_duplicate() {
        let s = two_copies();
        for what in [
            "pull game",
            "pull save",
            "delete-on-device game",
            "delete-on-device save",
        ] {
            let mut args = vec!["--json"];
            args.extend(what.split(' '));
            args.push("gba/RPG/Two.gba");
            assert_json_could_not_start(&s.run(&args), "duplicate_on_device");
        }
        assert!(s.device().join("Assets/gba/common/RPG/Two.gba").exists());
        assert!(s.device().join("Saves/gba/common/RPG/Two.sav").exists());
    }

    #[test]
    fn warns_of_skipped_saves_and_exits_0_when_pulling_all() {
        let s = two_copies();
        s.on_device("Assets/gba/common/Here.gba", b"here");
        s.on_device("Saves/gba/common/Here.sav", b"save");
        let ran = s.run(&["pull", "save"]);
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
        let s = Setup::new();
        s.in_library("roms/gba/Mine.gba", b"mine");
        let kept = s.in_library("saves/gba/Mine/a.sav", b"kept");
        set_written_at(&kept, 1_600_000_000);
        s.on_device("Assets/gba/common/Mine.gba", b"mine");
        s
    }

    #[test]
    fn pushes_version_at_given_time() {
        let s = with_a_version();
        let ran = s.run(&[
            "push",
            "save",
            "gba/Mine.gba",
            "--at",
            "2020-09-13T12-26-40",
        ]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            ran.stdout,
            "gba/Mine.gba: save 2020-09-13T12-26-40 pushed\n"
        );
        assert_eq!(
            fs::read(s.device().join("Saves/gba/common/Mine.sav")).unwrap(),
            b"kept"
        );
    }

    #[test]
    fn exits_1_when_no_version_at_given_time() {
        let s = with_a_version();
        let ran = s.run(&["push", "save", "gba/Mine.gba", "--at", "never"]);
        assert_eq!(ran.code, 1);
        assert_eq!(ran.stdout, "gba/Mine.gba: the library has no such save\n");
        let ran = s.run(&["--json", "trash", "save", "gba/Mine.gba", "--at", "never"]);
        assert_eq!(ran.code, 1);
        assert_eq!(ran.json()["data"][0]["version"], "never");
    }
}

mod importing {
    use super::*;

    /// A game in the library, no device, and a save in a folder apart from them.
    fn with_a_save_to_import() -> (Setup, PathBuf) {
        let s = Setup::new();
        s.in_library("roms/gba/Mine.gba", b"mine");
        let file = s.elsewhere("Mine.sav", b"save");
        set_written_at(&file, 1_600_000_000);
        (s, file)
    }

    /// `pocketeer --library … <args>`: importing needs no device.
    fn without_a_device(s: &Setup, args: &[&str]) -> Ran {
        run_and_capture(
            pocketeer_command()
                .arg("--library")
                .arg(s.library())
                .args(args),
        )
    }

    #[test]
    fn imports_games_under_dir() {
        let s = Setup::new();
        s.elsewhere("roms/old/Deep.gb", b"deep");
        s.elsewhere("roms/README.txt", b"read me");
        let folder = s.elsewhere("roms/Mine.gba", b"mine");
        let folder = folder.parent().unwrap().to_str().unwrap();
        let ran = without_a_device(&s, &["import", "game", folder]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            ran.stdout,
            "Mine.gba: imported as gba/Mine.gba\nDeep.gb: imported as gb/Deep.gb\n"
        );
    }

    #[test]
    fn prints_imported_save_as_json() {
        let (s, file) = with_a_save_to_import();
        let file = file.to_str().unwrap();
        let ran = without_a_device(&s, &["--json", "import", "save", "gba/Mine.gba", file]);
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
    fn prints_imported_game_as_json() {
        let s = Setup::new();
        let file = s.elsewhere("mine.gba", b"mine");
        let path = file.to_str().unwrap();
        let ran = without_a_device(&s, &["--json", "import", "game", path]);
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
        let s = Setup::new();
        let file = s.elsewhere("mine.bin", b"mine");
        let path = file.to_str().unwrap();
        let ran = without_a_device(&s, &["import", "game", path]);
        assert_eq!(ran.code, 1);
        assert_eq!(
            ran.stdout,
            "mine.bin: can't tell which platform it is for\n"
        );
        let ran = without_a_device(&s, &["import", "game", "--platform", "genesis", path]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert!(s.library().join("roms/genesis/mine.bin").exists());
    }

    #[test]
    fn exits_2_for_unlisted_platform_id() {
        let s = Setup::new();
        let file = s.elsewhere("mine.bin", b"mine");
        let path = file.to_str().unwrap();
        let absolute = s.dir.path().join("absolute");
        for platform in ["../../escaped", absolute.to_str().unwrap(), "arduboy"] {
            let args = ["--json", "import", "game", "--platform", platform, path];
            assert_json_could_not_start(&without_a_device(&s, &args), "unknown_platform");
        }
        assert!(!s.dir.path().join("escaped").exists());
        assert!(!absolute.exists());
    }

    #[test]
    fn logs_absolute_path_for_relative_path() {
        let (s, file) = with_a_save_to_import();
        let folder = file.parent().unwrap();
        let game = put(folder, "mine.gba", b"game");
        let in_folder = |args: &[&str]| {
            run_and_capture(
                pocketeer_command()
                    .current_dir(folder)
                    .arg("--library")
                    .arg(s.library())
                    .args(args),
            )
        };
        in_folder(&["import", "save", "gba/Mine.gba", "Mine.sav"]);
        in_folder(&["import", "game", "mine.gba"]);
        // The real paths: the temporary folder may be behind a symlink.
        let logged = s.log_lines();
        assert_eq!(logged[0]["from"], json!(fs::canonicalize(&file).unwrap()));
        assert_eq!(logged[1]["from"], json!(fs::canonicalize(&game).unwrap()));
    }
}

mod deleting {
    use super::*;

    #[test]
    fn deletes_game_and_leaves_its_save() {
        let s = Setup::new();
        let game = s.on_device("Assets/gba/common/One.gba", b"one");
        let save = s.on_device("Saves/gba/common/One.sav", b"save");
        let ran = s.run(&["delete-on-device", "game", "gba/One.gba"]);
        assert_eq!(ran.code, 0);
        assert_eq!(ran.stdout, "gba/One.gba: deleted from the device\n");
        assert!(!game.exists() && save.exists());
    }

    #[test]
    fn deletes_orphan_save() {
        let s = Setup::new();
        let save = s.on_device("Saves/gba/common/One.sav", b"save");
        let ran = s.run(&["delete-on-device", "orphan-save", "gba/One.sav"]);
        assert_eq!(ran.code, 0);
        assert_eq!(ran.stdout, "gba/One.sav: save deleted from the device\n");
        assert!(!save.exists());
    }

    #[test]
    fn exits_2_for_save_that_is_not_orphan() {
        let s = Setup::new();
        s.on_device("Assets/gba/common/One.gba", b"one");
        let save = s.on_device("Saves/gba/common/One.sav", b"save");
        let root = s.on_device("Analogue_Pocket.json", b"{}");
        let orphan = s.on_device("Saves/gba/common/Gone.sav", b"orphan");
        let in_library = s.in_library("saves/gba/Gone/a.sav", b"orphan");
        for name in ["gba/One.sav", "gba/../../../Analogue_Pocket.json"] {
            let ran = s.run(&["--json", "delete-on-device", "orphan-save", name]);
            assert_json_could_not_start(&ran, "not_an_orphan_save");
        }
        assert!(save.exists() && root.exists() && orphan.exists());
        assert_json_could_not_start(
            &s.run(&["--json", "trash", "orphan-save", "gba/One"]),
            "not_an_orphan_save",
        );
        assert!(in_library.exists());
    }
}

mod log {
    use super::*;

    #[test]
    fn logs_command_with_cli_front_end() {
        let s = Setup::new();
        s.on_device("Assets/gba/common/One.gba", b"one");
        s.run(&["pull", "game"]);
        let lines = s.log_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["front_end"], "cli");
        assert_eq!(lines[0]["game"], "gba/One.gba");
        assert_eq!(lines[0]["library"], "gba/One.gba");
        assert!(lines[0]["time"].is_string());
    }

    #[test]
    fn stops_batch_and_exits_1_when_log_unwritable() {
        let s = Setup::new();
        s.on_device("Assets/gba/common/One.gba", b"one");
        s.on_device("Assets/gba/common/Two.gba", b"two");
        s.break_the_log();
        let ran = s.run(&["pull", "game"]);
        assert_eq!(ran.code, 1);
        assert_eq!(ran.stdout, "gba/One.gba: pulled as gba/One.gba\n");
        assert!(
            ran.stderr.starts_with("error: couldn't write the log: "),
            "{}",
            ran.stderr
        );
        assert!(!s.library().join("roms/gba/Two.gba").exists());
    }

    #[test]
    fn reports_unwritable_log_in_json() {
        let s = Setup::new();
        s.on_device("Assets/gba/common/One.gba", b"one");
        s.on_device("Assets/gba/common/Two.gba", b"two");
        s.break_the_log();
        let ran = s.run(&["--json", "pull", "game"]);
        assert_eq!(ran.code, 1);
        let value = ran.json();
        assert_eq!(value["ok"], false);
        assert_eq!(value["data"].as_array().unwrap().len(), 1);
        assert!(ran.stderr.starts_with("error: couldn't write the log: "));
    }
}

#[test]
fn aligns_sizes_to_the_right() {
    let s = Setup::new();
    let long = s.on_device("Saves/gba/common/A.sav", &[0; 12]);
    let short = s.on_device("Saves/gba/common/B.sav", &[0; 3]);
    set_written_at(&long, 1_600_000_000);
    set_written_at(&short, 1_600_000_000);
    let ran = s.run(&["orphan-saves", "device"]);
    assert_eq!(
        ran.stdout,
        "SAVE           SIZE  WRITTEN\n\
         gba/A.sav  12 bytes  2020-09-13T12-26-40\n\
         gba/B.sav   3 bytes  2020-09-13T12-26-40\n"
    );
}

mod rename {
    use super::*;

    #[test]
    fn prints_library_rename_as_json() {
        let s = Setup::new();
        s.in_library("roms/gba/Old.gba", b"hello");
        s.in_library("saves/gba/Old/x.sav", b"save");
        let ran = s.run(&["--json", "rename", "library", "gba/Old.gba", "New"]);
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
        assert!(s.library().join("saves/gba/New/x.sav").exists());
    }

    #[test]
    fn renames_game_on_device_with_its_save() {
        let s = Setup::new();
        s.on_device("Assets/gba/common/RPG/Old.gba", b"hello");
        s.on_device("Saves/gba/common/RPG/Old.sav", b"save");
        let ran = s.run(&["rename", "device", "gba/RPG/Old.gba", "New"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.stdout, "gba/RPG/Old.gba: renamed to gba/RPG/New.gba\n");
        assert!(s.device().join("Saves/gba/common/RPG/New.sav").exists());
    }
}

mod bios {
    use super::*;

    /// A device with a core that loads `gba_bios.bin` of 4 bytes.
    fn with_core() -> Setup {
        let s = Setup::new();
        s.on_device(
            "Cores/a.GBA/core.json",
            br#"{"core":{"metadata":{"version":"1","platform_ids":["gba"]}}}"#,
        );
        s.on_device(
            "Cores/a.GBA/data.json",
            br#"{"data":{"data_slots":[{"filename":"gba_bios.bin","required":true,"parameters":"0x88","size_exact":4}]}}"#,
        );
        s
    }

    #[test]
    fn prints_list_as_json() {
        let s = with_core();
        assert_eq!(
            s.run(&["--json", "bios", "list"]).json(),
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
        let s = with_core();
        s.on_device("Assets/gba/common/gba_bios.bin", b"five!");
        let ran = s.run(&["bios", "list"]);
        assert_eq!(ran.code, 0);
        assert_eq!(
            ran.stdout,
            "FILE              STATUS                      NEEDED BY\n\
             gba/gba_bios.bin  wrong size: 5 bytes, not 4  a.GBA\n"
        );
    }

    #[test]
    fn puts_file_on_device() {
        let s = with_core();
        let from = s.elsewhere("mine.bin", b"four");
        let ran = s.run(&["bios", "put", "gba/gba_bios.bin", from.to_str().unwrap()]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.stdout, "gba/gba_bios.bin: put on the device\n");
        let on_device = s.device().join("Assets/gba/common/gba_bios.bin");
        assert_eq!(fs::read(on_device).unwrap(), b"four");
    }

    #[test]
    fn exits_2_for_file_no_core_loads() {
        let s = with_core();
        let from = s.elsewhere("mine.bin", b"four");
        let from = from.to_str().unwrap();
        let ran = s.run(&["--json", "bios", "put", "gba/other.bin", from]);
        assert_eq!(ran.code, 2);
        assert_eq!(ran.json()["error"]["code"], "not_a_bios_file");
    }
}

mod cores {
    use super::*;

    const GB: &str = "Spiritualized.GB";

    /// Answers every request with the zip of `Spiritualized.GB` at `version`,
    /// so that nothing goes online; its address.
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

    /// A library whose inventory, just written so that it isn't asked for
    /// again, has `Spiritualized.GB` at 2.0 at `url`.
    fn with_inventory(url: &str) -> Setup {
        let s = Setup::new();
        let inventory = json!([{
            "id": GB,
            "version": "2.0",
            "platforms": ["gb"],
            "download_url": url,
        }]);
        s.in_library("cores.json", inventory.to_string().as_bytes());
        s
    }

    fn core_on_device(s: &Setup, id: &str, version: &str) {
        let json =
            format!(r#"{{"core":{{"metadata":{{"version":"{version}","platform_ids":["gb"]}}}}}}"#);
        s.on_device(&format!("Cores/{id}/core.json"), json.as_bytes());
    }

    #[test]
    fn prints_list_as_json() {
        let s = with_inventory("http://127.0.0.1:1/never");
        core_on_device(&s, GB, "1.0");
        core_on_device(&s, "someone.Else", "3.0");
        assert_eq!(
            s.run(&["--json", "cores", "list"]).json(),
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
        let s = with_inventory("http://127.0.0.1:1/never");
        core_on_device(&s, GB, "1.0");
        let ran = s.run(&["cores", "list"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            ran.stdout,
            "CORE              PLATFORM  ON THE DEVICE  LATEST\n\
             Spiritualized.GB  gb        1.0            2.0\n"
        );
    }

    #[test]
    fn prints_install_as_json_with_entry_for_each_core() {
        let s = with_inventory(&serve_release("2.0"));
        assert_eq!(
            s.run(&["--json", "cores", "install", GB]).json(),
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
        assert!(s.device().join("Cores").join(GB).join("core.json").exists());
    }

    #[test]
    fn exits_2_for_unsupported_core_and_installs_nothing() {
        let s = with_inventory(&serve_release("2.0"));
        assert_json_could_not_start(
            &s.run(&["--json", "cores", "install", GB, "no.Such"]),
            "unsupported_core",
        );
        assert!(!s.device().join("Cores").join(GB).exists());
    }

    #[test]
    fn exits_1_when_download_fails() {
        let s = with_inventory("http://127.0.0.1:1/never");
        let ran = s.run(&["cores", "install", GB]);
        assert_eq!(ran.code, 1);
        assert!(
            ran.stdout.starts_with("Spiritualized.GB: error: "),
            "{}",
            ran.stdout
        );
    }

    #[test]
    fn updates_cores_with_newer_release_when_none_named() {
        let s = with_inventory(&serve_release("2.0"));
        core_on_device(&s, GB, "1.0");
        core_on_device(&s, "someone.Else", "3.0");
        let ran = s.run(&["cores", "update"]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.stdout, "Spiritualized.GB: 2.0 installed\n");
        let ran = s.run(&["cores", "update"]);
        assert_eq!(ran.stdout, "every core is up to date\n");
    }

    #[test]
    fn exits_2_for_update_or_remove_of_core_not_on_device() {
        let s = with_inventory("http://127.0.0.1:1/never");
        for what in ["update", "remove"] {
            assert_json_could_not_start(
                &s.run(&["--json", "cores", what, "../Assets"]),
                "core_not_on_device",
            );
        }
        assert!(s.device().join("Assets").exists());
    }

    #[test]
    fn removes_core_from_device() {
        let s = with_inventory("http://127.0.0.1:1/never");
        core_on_device(&s, GB, "1.0");
        let ran = s.run(&["cores", "remove", GB]);
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(ran.stdout, "Spiritualized.GB: removed from the device\n");
        assert!(!s.device().join("Cores").join(GB).exists());
    }
}
