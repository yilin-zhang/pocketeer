//! End-to-end: covers status / fetch against a mock libretro host.

mod common;

use common::Harness;
use std::fs;

#[test]
fn fetch_and_status() {
    let env = Harness::new();
    env.json(&["library", "init"]);
    let src = env.tmp.path().join("roms");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("Hit (USA).gba"), "hello world").unwrap();
    fs::write(src.join("Miss (USA).gba"), "x").unwrap();
    env.json(&[
        "library",
        "import",
        src.to_str().unwrap(),
        "--platform",
        "gba",
    ]);

    let (v, code) = env.json(&["covers", "status"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["platforms"][0]["games"], 2);
    assert_eq!(v["data"]["platforms"][0]["with_cover"], 0);
    assert_eq!(v["data"]["platforms"][0]["supported"], true);

    let mut server = mockito::Server::new();
    let hit = server
        .mock(
            "GET",
            "/Nintendo_-_Game_Boy_Advance/master/Named_Boxarts/Hit%20%28USA%29.png",
        )
        .with_body(b"PNG")
        .create();
    let miss = server
        .mock(
            "GET",
            "/Nintendo_-_Game_Boy_Advance/master/Named_Boxarts/Miss%20%28USA%29.png",
        )
        .with_status(404)
        .create();
    let (v, code) = env.json_online(
        &["covers", "fetch"],
        &[("POCKETEER_THUMBNAILS_BASE", &server.url())],
    );
    assert_eq!(code, 0, "{v}");
    hit.assert();
    miss.assert();
    assert_eq!(v["data"]["summary"]["fetched"], 1);
    assert_eq!(v["data"]["summary"]["not_found"], 1);
    assert_eq!(
        fs::read(env.lib.join("covers/gba/Hit (USA).png")).unwrap(),
        b"PNG"
    );

    let (v, _) = env.json(&["covers", "status"]);
    assert_eq!(v["data"]["platforms"][0]["with_cover"], 1);
    assert_eq!(v["data"]["platforms"][0]["not_found"], 1);
}
