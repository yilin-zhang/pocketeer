//! Shared harness for the end-to-end CLI tests: a throwaway card copy,
//! library, cache and source folder in one temp dir, and one way to run the
//! binary that never touches the user's real card, config or network.

#![allow(dead_code)]

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use pocketeer_core::{
    catalog::inventory::{CORES_URL, PLATFORMS_URL},
    net::Cache,
};
use serde_json::Value;
use zip::write::SimpleFileOptions;

pub const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures");

pub fn fixture(rel: &str) -> String {
    format!("{FIXTURES}/{rel}")
}

/// The test DAT: Hello World (USA), Quick Fox (Europe), Two Discs (Japan).
pub fn dat() -> String {
    fixture("roms/test.dat")
}

pub fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

pub fn build_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, contents) in files {
        w.start_file(*name, SimpleFileOptions::default()).unwrap();
        w.write_all(contents).unwrap();
    }
    w.finish().unwrap().into_inner()
}

pub struct Harness {
    pub tmp: tempfile::TempDir,
    /// Card root (`--root`). Only exists after [`Harness::with_card`]; before
    /// that it points nowhere, so commands see "no card".
    pub sd: PathBuf,
    /// Library folder (`--library`); created by `library init`.
    pub lib: PathBuf,
    /// A folder for ROM files to import or sync from.
    pub src: PathBuf,
    /// Cache dir (`--cache-dir`).
    pub cache: PathBuf,
    offline: bool,
}

impl Harness {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        Harness {
            sd: tmp.path().join("sd"),
            lib: tmp.path().join("lib"),
            src,
            cache: tmp.path().join("cache"),
            tmp,
            offline: true,
        }
    }

    /// A copy of the fixture card (two cores, junk, instance JSON).
    pub fn with_card(self) -> Self {
        copy_dir(&Path::new(FIXTURES).join("sd-basic"), &self.sd);
        self
    }

    /// The fixture inventory, pre-cached so `--offline` runs can resolve cores.
    pub fn with_inventory(self) -> Self {
        let cache = Cache::new(&self.cache);
        for (key, url, file) in [
            ("cores.json", CORES_URL, "inventory/cores.json"),
            ("platforms.json", PLATFORMS_URL, "inventory/platforms.json"),
        ] {
            cache
                .store(key, url, &fs::read(fixture(file)).unwrap(), None)
                .unwrap();
        }
        self
    }

    fn command(&self, json: bool, args: &[&str], online: bool, envs: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pocketeer"));
        if !online {
            cmd.arg("--offline");
        }
        if json {
            cmd.arg("--json");
        }
        cmd.args(["--cache-dir", self.cache.to_str().unwrap()])
            .args(["--root", self.sd.to_str().unwrap()])
            .args(["--library", self.lib.to_str().unwrap()])
            .args(args)
            .env_remove("POCKETEER_ROOT")
            .env_remove("POCKETEER_LIBRARY")
            .env_remove("POCKETEER_GITHUB_TOKEN")
            .env(
                "POCKETEER_CONFIG",
                "/nonexistent/pocketeer-test-config.toml",
            );
        for (k, v) in envs {
            cmd.env(k, v);
        }
        cmd
    }

    pub fn run(&self, json: bool, args: &[&str]) -> Output {
        self.command(json, args, !self.offline, &[])
            .output()
            .unwrap()
    }

    /// `--json` run: the envelope and the exit code.
    pub fn json(&self, args: &[&str]) -> (Value, i32) {
        parse(&self.run(true, args))
    }

    /// A run with the flags exactly as given (`--json` may be among them).
    pub fn raw(&self, args: &[&str]) -> Output {
        self.run(false, args)
    }

    /// Stdout as text, with the card's temp path replaced so snapshots are stable.
    pub fn text_of(&self, out: &Output) -> String {
        String::from_utf8(out.stdout.clone())
            .unwrap()
            .replace(self.sd.to_str().unwrap(), "[fixtures]/sd-basic")
    }

    /// Same, without `--offline` and with extra environment variables (for
    /// tests that point the binary at a mock server).
    pub fn json_online(&self, args: &[&str], envs: &[(&str, &str)]) -> (Value, i32) {
        parse(&self.command(true, args, true, envs).output().unwrap())
    }

    /// Writes a file on the card.
    pub fn write(&self, rel: &str, body: &str) {
        write_at(&self.sd, rel, body);
    }

    /// Writes a file in the source folder.
    pub fn write_src(&self, rel: &str, body: impl AsRef<[u8]>) {
        write_at(&self.src, rel, body);
    }

    pub fn read(&self, rel: &str) -> Option<String> {
        fs::read_to_string(self.sd.join(rel)).ok()
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.sd.join(rel).exists()
    }

    /// A zip in the temp dir, returned by path.
    pub fn write_zip(&self, name: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let path = self.tmp.path().join(name);
        fs::write(&path, build_zip(files)).unwrap();
        path
    }
}

fn write_at(root: &Path, rel: &str, body: impl AsRef<[u8]>) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

pub fn parse(out: &Output) -> (Value, i32) {
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "bad json ({e}):\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (v, out.status.code().unwrap())
}
