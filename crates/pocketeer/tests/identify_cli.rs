//! End-to-end: `library identify` against a mock libretro DAT host.

mod common;
use common::Harness;

#[test]
fn identify_retries_after_a_failure_then_uses_its_cached_dat() {
    let env = Harness::new();
    env.json(&["library", "init"]);
    env.write_src("local.gba", b"hello world");
    let (v, code) = env.json(&[
        "library",
        "import",
        env.src.to_str().unwrap(),
        "--platform",
        "gba",
    ]);
    assert_eq!(code, 0, "{v}");
    let mut server = mockito::Server::new();
    let base = [("POCKETEER_DAT_BASE", server.url())];
    let base: Vec<(&str, &str)> = base.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let dat = "/Nintendo%20-%20Game%20Boy%20Advance.dat";

    // A failed download is not a "no match": the next run tries again.
    let failed = server.mock("GET", dat).with_status(503).create();
    let (v, code) = env.json_online(&["library", "identify"], &base);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["data"]["no_match"], 0);
    failed.assert();
    failed.remove();

    let ok = server
        .mock("GET", dat)
        .with_body(include_bytes!("../../../tests/fixtures/roms/test.dat"))
        .create();
    let (v, code) = env.json_online(&["library", "identify"], &base);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["matched"], 1);
    ok.assert();

    // With the DAT gone from the host, the cached copy answers, and
    // nothing changes again.
    ok.remove();
    let (v, code) = env.json_online(&["library", "identify"], &base);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["unchanged"], 1);
    assert_eq!(
        std::fs::read(env.lib.join("roms/gba/local.gba")).unwrap(),
        b"hello world",
        "identifying never touches the file"
    );
}
