mod common;
use common::Harness;
use pocketeer_core::{catalog::libretro, net::Cache};

#[test]
fn identify_existing_library_uses_cached_dat_and_preserves_files() {
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
    let system = pocketeer_core::catalog::dat_system("gba").unwrap();
    Cache::new(&env.cache)
        .store(
            &libretro::dat_cache_key(system),
            &libretro::dat_url(libretro::DAT_BASE, system),
            include_bytes!("../../../tests/fixtures/roms/test.dat"),
            None,
        )
        .unwrap();
    let (v, code) = env.json(&["library", "identify"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["matched"], 1);
    let (v, code) = env.json(&["library", "identify"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["unchanged"], 1);
    assert_eq!(
        std::fs::read(env.lib.join("roms/gba/local.gba")).unwrap(),
        b"hello world"
    );
    let output = env.raw(&["library", "standardize", "--dry-run"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Hello World"));
}

#[test]
fn network_failure_is_not_a_negative_match_and_next_run_retries() {
    let env = Harness::new();
    env.json(&["library", "init"]);
    env.write_src("local.gba", b"hello world");
    env.json(&[
        "library",
        "import",
        env.src.to_str().unwrap(),
        "--platform",
        "gba",
    ]);
    let mut server = mockito::Server::new();
    let failed = server
        .mock("GET", "/Nintendo%20-%20Game%20Boy%20Advance.dat")
        .with_status(503)
        .create();
    let (v, code) = env.json_online(
        &["library", "identify"],
        &[("POCKETEER_DAT_BASE", &server.url())],
    );
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["data"]["no_match"], 0);
    failed.assert();
    failed.remove();
    let success = server
        .mock("GET", "/Nintendo%20-%20Game%20Boy%20Advance.dat")
        .with_body(include_bytes!("../../../tests/fixtures/roms/test.dat"))
        .create();
    let (v, code) = env.json_online(
        &["library", "identify"],
        &[("POCKETEER_DAT_BASE", &server.url())],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["matched"], 1);
    success.assert();
}
