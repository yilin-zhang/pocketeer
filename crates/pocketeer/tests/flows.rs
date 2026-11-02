//! The main user journeys, end to end through the binary, checking the
//! bytes on disk at every step.

mod common;
use common::Harness;
use std::fs;

#[test]
fn a_game_goes_to_the_card_and_its_save_comes_back_after_it_is_lost() {
    let env = Harness::new().with_card();
    assert_eq!(env.json(&["library", "init"]).1, 0);
    env.write_src("Hello World (USA).gba", "the game");

    let (v, code) = env.json(&[
        "library",
        "import",
        env.src.to_str().unwrap(),
        "--platform",
        "gba",
    ]);
    assert_eq!(code, 0, "{v}");

    let (v, code) = env.json(&["card", "copy", "Hello World (USA).gba"]);
    assert_eq!(code, 0, "{v}");
    let rom = env.sd.join("Assets/gba/common/Hello World (USA).gba");
    assert_eq!(fs::read(&rom).unwrap(), b"the game");

    // Playing it on the Pocket writes a save next to the ROM's name.
    let save = "Saves/gba/common/Hello World (USA).sav";
    env.write(save, "level 3");
    let (v, code) = env.json(&["saves", "backup"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        fs::read(env.lib.join("saves").join(save)).unwrap(),
        b"level 3"
    );

    // The card's copy is lost; the library's backup puts it back.
    fs::remove_file(env.sd.join(save)).unwrap();
    let (v, code) = env.json(&["saves", "restore", save]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(fs::read(env.sd.join(save)).unwrap(), b"level 3");
    assert_eq!(
        fs::read(&rom).unwrap(),
        b"the game",
        "the ROM was never touched"
    );
}

#[test]
fn paths_that_leave_their_folder_are_refused() {
    let env = Harness::new().with_card();
    assert_eq!(env.json(&["library", "init"]).1, 0);
    env.write("Assets/gba/common/game.gba", "rom");
    for (args, why) in [
        (vec!["saves", "restore", "../outside.sav"], "INVALID_PATH"),
        (vec!["saves", "restore", "/etc/passwd"], "INVALID_PATH"),
        (
            vec!["saves", "trash", "Saves/../../outside.sav"],
            "INVALID_PATH",
        ),
        (
            vec![
                "card",
                "rename",
                "Assets/gba/common/game.gba",
                "../game.gba",
            ],
            "INVALID_NAME",
        ),
    ] {
        let (v, code) = env.json(&args);
        assert_eq!(code, 1, "{args:?} was accepted: {v}");
        assert_eq!(v["error"]["code"], why, "{args:?}");
    }
    // Card import only takes the card's own ROMs; anything else is not found.
    let (v, code) = env.json(&["card", "import", "../outside.gba"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["data"]["not_found"][0], "../outside.gba");
    assert!(env.sd.join("Assets/gba/common/game.gba").is_file());
}
