//! The CLI over the invented MCPEvol-Bench miniature. No network, no real benchmark data.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn mini_data() -> PathBuf {
    repo_root().join("fixtures/mcpevol-mini/data")
}

fn harness(args: &[&str], with_env: bool) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_harness"));
    cmd.args(args)
        .env_remove("MCP_DATA_DIR")
        .env_remove("EVAL_DATA_DIR");
    if with_env {
        cmd.env("MCP_DATA_DIR", mini_data())
            .env("EVAL_DATA_DIR", mini_data());
    }
    cmd.output().expect("run harness")
}

fn config_path() -> String {
    repo_root()
        .join("fixtures/mcpevol-mini.toml")
        .to_string_lossy()
        .into_owned()
}

#[test]
fn validate_prints_the_mcpevol_report() {
    let output = harness(&["validate", "--config", &config_path()], true);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for needle in [
        "adapter mcpevol",
        "brief expected: 123 servers / 1272 tools / 201 tasks",
        "Early=original",
        "test_cases/status_dict.json",
        "unresolved server name",
    ] {
        assert!(text.contains(needle), "{needle} missing:\n{text}");
    }
}

#[test]
fn validate_without_the_env_vars_fails_and_names_them() {
    let output = harness(&["validate", "--config", &config_path()], false);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("MCP_DATA_DIR is not set"), "{stderr}");
    assert!(stderr.contains("EVAL_DATA_DIR is not set"), "{stderr}");
}
