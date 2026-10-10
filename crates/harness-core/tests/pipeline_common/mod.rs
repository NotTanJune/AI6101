//! Shared helpers for the pipeline integration tests.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use harness_core::pipeline::{EnvLookup, RunOptions};
use harness_core::{Config, FixedClock, RunEvent, read_events};

/// Repository root (two levels above this crate).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// The checked-in smoke config, loaded and valid.
pub fn smoke_config() -> Config {
    let config = Config::load(repo_root().join("fixtures/smoke.toml")).expect("smoke config loads");
    assert_eq!(config.validate(), vec![], "smoke config validates");
    config
}

/// Environment lookup that knows nothing.
pub fn empty_env() -> EnvLookup {
    Arc::new(|_| None)
}

/// Deterministic options: fixed clock, empty environment.
pub fn opts(dir: &Path) -> RunOptions {
    RunOptions::new(dir)
        .with_clock(Arc::new(FixedClock::default()))
        .with_env(empty_env())
}

/// Events written to `<dir>/events.jsonl`, all schema-valid.
pub fn read_run_events(dir: &Path) -> Vec<RunEvent> {
    read_events(dir.join("events.jsonl")).expect("events.jsonl is schema-valid")
}

pub fn read_text(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path.as_ref()).unwrap_or_else(|e| panic!("{}: {e}", path.as_ref().display()))
}

/// Every regular file below `dir`, sorted.
pub fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Copy a directory tree.
pub fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("mkdir");
    for entry in fs::read_dir(from).expect("read_dir") {
        let path = entry.expect("entry").path();
        let target = to.join(path.file_name().expect("name"));
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            fs::copy(&path, &target).expect("copy");
        }
    }
}
