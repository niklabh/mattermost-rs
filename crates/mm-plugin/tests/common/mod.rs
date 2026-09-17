//! Shared by the mm-plugin suites: fixtures, the canonical rendering, and the Go oracle.

#![allow(dead_code)]
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use serde_json::{Map, Value as Json};

mod render;
pub use render::*;

/// `reference/dump/plugingen`, built once per test process.
pub fn plugingen() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugingen");
        let status = Command::new("go")
            .args(["build", "-o"])
            .arg(&out)
            .arg("./plugingen")
            .current_dir(root().join("reference/dump"))
            .status()
            .unwrap_or_else(|e| panic!("the Go oracle needs a Go toolchain on PATH: {e}"));
        assert!(status.success(), "building reference/dump/plugingen failed");
        out
    })
}

pub fn run_plugingen(args: &[&std::ffi::OsStr]) -> Vec<u8> {
    let out = Command::new(plugingen())
        .args(args)
        // plugingen reads the Go tree relative to reference/dump.
        .current_dir(root().join("reference/dump"))
        .env("TZ", "Asia/Kolkata")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "plugingen {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

pub fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn expected() -> Map<String, Json> {
    let text = std::fs::read_to_string(fixtures().join("gob/expected.json")).unwrap_or_else(|e| {
        panic!("{e}; regenerate with `cd reference/dump && TZ=Asia/Kolkata go run ./plugingen gob ../../fixtures/plugin/gob`")
    });
    serde_json::from_str(&text).unwrap()
}

pub fn report(failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}
