//! The `harness` binary: help, validate and run against the checked-in smoke fixtures.
//! No network: the smoke config uses the mock provider and the fixture runner.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use harness_cli::commands::{TuiOptions, build_tui_session};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn smoke() -> PathBuf {
    repo_root().join("fixtures/smoke.toml")
}

fn harness(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("run harness")
}

fn s(text: &str) -> &std::ffi::OsStr {
    std::ffi::OsStr::new(text)
}

fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn help_lists_the_three_commands() {
    let output = harness(&[s("--help")]);
    assert!(output.status.success());
    let text = out(&output);
    for command in ["tui", "validate", "run"] {
        assert!(
            text.contains(command),
            "--help should list {command}:\n{text}"
        );
    }
}

#[test]
fn tui_help_lists_every_flag_of_the_coding_agent() {
    let output = harness(&[s("tui"), s("--help")]);
    assert!(output.status.success(), "{}", err(&output));
    let text = out(&output);
    for flag in [
        "--workspace",
        "--model",
        "--effort",
        "--models",
        "--mcp-config",
        "--kb",
        "--sessions-dir",
        "--budget-usd",
        "--mock-script",
    ] {
        assert!(text.contains(flag), "{flag} missing from:\n{text}");
    }
    assert!(text.contains("[default: luna]"), "{text}");
    assert!(text.contains("[default: 5]"), "{text}");
    for gone in ["--config", "--runs-dir"] {
        assert!(
            !text.contains(gone),
            "the old experiment-console flag {gone} is gone:\n{text}"
        );
    }
}

fn tui_cmd(args: &[&std::ffi::OsStr]) -> Output {
    // Errors must come before the terminal is touched, so these runs never enter raw mode.
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("tui")
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .output()
        .expect("run harness")
}

fn models_path() -> PathBuf {
    repo_root().join("configs/models.toml")
}

#[test]
fn tui_without_the_api_key_names_the_variable_and_exits_before_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let output = tui_cmd(&[
        s("--workspace"),
        dir.path().as_os_str(),
        s("--models"),
        models_path().as_os_str(),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        err(&output).trim(),
        "error: environment variable OPENAI_API_KEY is not set"
    );
    assert!(out(&output).is_empty(), "{}", out(&output));

    // A Gemini model names the Gemini variable instead.
    let output = tui_cmd(&[
        s("--workspace"),
        dir.path().as_os_str(),
        s("--models"),
        models_path().as_os_str(),
        s("--model"),
        s("flash"),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        err(&output).trim(),
        "error: environment variable GEMINI_API_KEY is not set"
    );
}

#[test]
fn tui_rejects_bad_options_before_opening_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let models = models_path();
    let base = |extra: &[&std::ffi::OsStr]| {
        let mut args: Vec<&std::ffi::OsStr> = vec![
            s("--workspace"),
            dir.path().as_os_str(),
            s("--models"),
            models.as_os_str(),
        ];
        args.extend_from_slice(extra);
        tui_cmd(&args)
    };
    let unknown = base(&[s("--model"), s("gpt-9")]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        err(&unknown).contains("unknown model alias \"gpt-9\""),
        "{}",
        err(&unknown)
    );
    assert!(
        err(&unknown).contains("luna, sol, astra, flash, pro"),
        "{}",
        err(&unknown)
    );

    let effort = base(&[s("--model"), s("sol"), s("--effort"), s("none")]);
    assert_eq!(effort.status.code(), Some(1));
    assert!(
        err(&effort).contains("allowed: low, medium, high, xhigh, max"),
        "{}",
        err(&effort)
    );

    let budget = base(&[s("--budget-usd"), s("0")]);
    assert_eq!(budget.status.code(), Some(1));
    assert!(err(&budget).contains("--budget-usd"), "{}", err(&budget));

    let missing = tui_cmd(&[s("--workspace"), dir.path().join("nope").as_os_str()]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        err(&missing).contains("is not a directory"),
        "{}",
        err(&missing)
    );

    let registry = tui_cmd(&[
        s("--workspace"),
        dir.path().as_os_str(),
        s("--models"),
        dir.path().join("missing.toml").as_os_str(),
    ]);
    assert_eq!(registry.status.code(), Some(1));
    assert!(
        err(&registry).contains("cannot read model registry"),
        "{}",
        err(&registry)
    );
}

fn mock_options(workspace: &Path, sessions: &Path) -> TuiOptions {
    TuiOptions {
        workspace: Some(workspace.to_path_buf()),
        models: Some(models_path()),
        sessions_dir: Some(sessions.to_path_buf()),
        mock_script: Some(repo_root().join("fixtures/agent/hello")),
        ..TuiOptions::default()
    }
}

fn theme_notices(app: &harness_tui::App) -> Vec<String> {
    app.transcript
        .iter()
        .filter_map(|e| match e {
            harness_tui::Entry::Notice(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn an_invalid_discovered_config_becomes_a_notice_and_the_session_still_builds() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let config = dir.path().join("config.json");
    fs::write(&config, "{ not json").unwrap();
    let options = TuiOptions {
        config_sources: vec![config.clone()],
        ..mock_options(dir.path(), &sessions)
    };
    let session = build_tui_session(&options, &|_| None).expect("session builds");
    let notices = theme_notices(&session.app);
    assert!(
        notices
            .iter()
            .any(|n| n.contains("invalid JSON") && n.contains("config.json")),
        "{notices:?}"
    );
    assert_eq!(session.app.theme_spec.label(), "tokyo-night");
}

#[test]
fn an_unknown_theme_is_an_error_from_the_session_builder() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let options = TuiOptions {
        theme: Some("nope".to_owned()),
        ..mock_options(dir.path(), &sessions)
    };
    let err = build_tui_session(&options, &|_| None)
        .err()
        .expect("unknown theme is an error");
    assert!(
        err.contains("nope") && err.contains("gruvbox-dark-hard"),
        "{err}"
    );
}

#[test]
fn the_theme_environment_variable_applies_and_the_flag_wins() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let nord_env = |name: &str| (name == "MCPE_THEME").then(|| "nord".to_owned());
    let options = mock_options(dir.path(), &sessions);
    let session = build_tui_session(&options, &nord_env).expect("session builds");
    assert_eq!(session.app.theme_spec.label(), "nord");

    let flagged = TuiOptions {
        theme: Some("dracula".to_owned()),
        ..mock_options(dir.path(), &sessions)
    };
    let session = build_tui_session(&flagged, &nord_env).expect("session builds");
    assert_eq!(session.app.theme_spec.label(), "dracula");
}

#[test]
fn a_missing_config_file_or_unknown_theme_exits_1_before_the_terminal_opens() {
    let dir = tempfile::tempdir().unwrap();
    let mock = repo_root().join("fixtures/agent/hello");
    let unknown = mcpe_cmd(
        &[
            s("--theme"),
            s("nope"),
            s("--mock-script"),
            mock.as_os_str(),
            s("--workspace"),
            dir.path().as_os_str(),
        ],
        dir.path(),
    );
    assert_eq!(unknown.status.code(), Some(1), "{}", err(&unknown));
    assert!(err(&unknown).contains("nope") && err(&unknown).contains("gruvbox-dark-hard"));

    let missing = mcpe_cmd(
        &[
            s("--config"),
            s("missing.json"),
            s("--mock-script"),
            mock.as_os_str(),
            s("--workspace"),
            dir.path().as_os_str(),
        ],
        dir.path(),
    );
    assert_eq!(missing.status.code(), Some(1), "{}", err(&missing));
    assert!(err(&missing).contains("missing.json"), "{}", err(&missing));
}

#[test]
fn the_tui_session_is_built_without_a_key_when_a_mock_script_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let options = TuiOptions {
        kb: Some(repo_root().join("fixtures/agent/calc-kb.json")),
        ..mock_options(dir.path(), &sessions)
    };
    let mut session = build_tui_session(&options, &|_| None).expect("session builds with no key");
    assert_eq!(session.app.status.model_alias, "luna");
    assert_eq!(
        session.app.status.effort,
        harness_agent::registry::Effort::Max
    );
    assert_eq!(session.app.status.cap_usd, 5.0);
    let aliases: Vec<&str> = session
        .app
        .models
        .iter()
        .map(|m| m.alias.as_str())
        .collect();
    assert_eq!(aliases, vec!["luna", "sol", "astra", "flash", "pro"]);
    assert!(session.app.mcp.is_empty());
    assert!(session.session_dir.starts_with(&sessions));

    // The runtime announces the KB it was given and writes the session start to the transcript.
    use harness_tui::AgentPort;
    let mut events = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !events
        .iter()
        .any(|e| matches!(e, harness_tui::UiEvent::KbSummary(_)))
    {
        events.extend(session.agent.poll());
        assert!(
            std::time::Instant::now() < deadline,
            "no KB summary: {events:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let summary = events.iter().find_map(|e| match e {
        harness_tui::UiEvent::KbSummary(k) => Some(k.clone()),
        _ => None,
    });
    let summary = summary.unwrap();
    assert_eq!(summary.kb_id, "calc-kb");
    assert_eq!(summary.servers, vec![("calc".to_owned(), 1)]);
    assert!(session.agent.shutdown(std::time::Duration::from_secs(5)));
    let transcript = fs::read_to_string(session.session_dir.join("transcript.jsonl")).unwrap();
    assert!(
        transcript.contains("\"schema\":\"agent-session/1\""),
        "{transcript}"
    );
    assert!(
        transcript.contains("\"kind\":\"session_start\""),
        "{transcript}"
    );
}

#[test]
fn the_default_sessions_directory_is_inside_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let options = TuiOptions {
        sessions_dir: None,
        ..mock_options(dir.path(), dir.path())
    };
    let mut session = build_tui_session(&options, &|_| None).expect("session");
    assert!(
        session
            .session_dir
            .starts_with(dir.path().join(".harness").join("sessions")),
        "{}",
        session.session_dir.display()
    );
    use harness_tui::AgentPort;
    assert!(session.agent.shutdown(std::time::Duration::from_secs(5)));
}

#[test]
fn a_missing_key_is_reported_by_name_only_when_no_mock_script_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let options = TuiOptions {
        mock_script: None,
        ..mock_options(dir.path(), dir.path())
    };
    let Err(message) = build_tui_session(&options, &|_| None) else {
        panic!("a session must not be built without a key");
    };
    assert_eq!(message, "environment variable OPENAI_API_KEY is not set");
    // A blank value counts as missing.
    let Err(message) = build_tui_session(&options, &|_| Some("  ".to_owned())) else {
        panic!("a blank key is no key");
    };
    assert_eq!(message, "environment variable OPENAI_API_KEY is not set");
}

#[test]
fn a_bad_kb_or_mcp_config_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let bad_kb = dir.path().join("kb.json");
    fs::write(&bad_kb, "{ not json").unwrap();
    let Err(message) = build_tui_session(
        &TuiOptions {
            kb: Some(bad_kb),
            ..mock_options(dir.path(), dir.path())
        },
        &|_| None,
    ) else {
        panic!("an unreadable KB must fail");
    };
    assert!(!message.is_empty());
    let bad_mcp = dir.path().join("mcp.json");
    fs::write(&bad_mcp, r#"{"mcpServers": {"x": {}}}"#).unwrap();
    let Err(message) = build_tui_session(
        &TuiOptions {
            mcp_config: Some(bad_mcp),
            ..mock_options(dir.path(), dir.path())
        },
        &|_| None,
    ) else {
        panic!("an invalid mcp.json must fail");
    };
    assert!(message.contains("needs a command or a url"), "{message}");
}

#[test]
fn run_help_documents_the_distinct_exit_codes() {
    let output = harness(&[s("run"), s("--help")]);
    assert!(output.status.success());
    let text = out(&output);
    for needle in [
        "failed",
        "cancelled",
        "budget",
        "3",
        "4",
        "5",
        "--output",
        "--force",
    ] {
        assert!(text.contains(needle), "{needle} missing from:\n{text}");
    }
}

#[test]
fn validate_smoke_config_succeeds_and_reports_the_setup() {
    let output = harness(&[s("validate"), s("--config"), smoke().as_os_str()]);
    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    let text = out(&output);
    for needle in [
        "1 selected",
        "tracker-create-high-priority",
        "provider: mock",
        "no_update, full_rewrite, selective_patch",
        "tool-kb/1",
        "run-event/1",
        "servers: 1",
    ] {
        assert!(text.contains(needle), "{needle} missing from:\n{text}");
    }
}

#[test]
fn validate_a_broken_config_fails_and_lists_every_problem() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.toml");
    let text = fs::read_to_string(smoke())
        .unwrap()
        .replace("root = \"smoke\"", "root = \"missing-inputs\"")
        .replace(
            "fixture_dir = \"smoke/runner\"",
            "fixture_dir = \"missing-runner\"",
        )
        .replace("cap_usd = 100.0", "cap_usd = -1.0");
    fs::write(&bad, text).unwrap();
    let output = harness(&[s("validate"), s("--config"), bad.as_os_str()]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = err(&output);
    for field in ["inputs.root", "runner.fixture_dir", "budget.cap_usd"] {
        assert!(stderr.contains(field), "{field} missing from:\n{stderr}");
    }
    assert!(
        out(&output).is_empty(),
        "no success report for an invalid config"
    );
}

#[test]
fn validate_unreadable_or_malformed_config_fails() {
    let dir = tempfile::tempdir().unwrap();
    let missing = harness(&[
        s("validate"),
        s("--config"),
        dir.path().join("nope.toml").as_os_str(),
    ]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        err(&missing).contains("cannot read config"),
        "{}",
        err(&missing)
    );

    let junk = dir.path().join("junk.toml");
    fs::write(&junk, "this is = not [valid").unwrap();
    let output = harness(&[s("validate"), s("--config"), junk.as_os_str()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(err(&output).contains("invalid config"), "{}", err(&output));
}

#[test]
fn run_smoke_config_exits_zero_and_writes_the_layout() {
    let dir = tempfile::tempdir().unwrap();
    let output_dir = dir.path().join("run");
    let output = harness(&[
        s("run"),
        s("--config"),
        smoke().as_os_str(),
        s("--output"),
        output_dir.as_os_str(),
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}\n{}",
        out(&output),
        err(&output)
    );
    let text = out(&output);
    for strategy in ["no_update", "full_rewrite", "selective_patch"] {
        assert!(text.contains(&format!("strategy {strategy}")), "{text}");
    }
    for needle in [
        "adaptation: unchanged",
        "adaptation: promoted",
        "canary: not_applicable",
        "canary: pass",
        "mock usage, not USD",
        "local dispatch cap",
        "status ok",
    ] {
        assert!(text.contains(needle), "{needle} missing from:\n{text}");
    }
    for file in [
        "manifest.toml",
        "events.jsonl",
        "summary.json",
        "kb/no_update-rep1.json",
        "kb/full_rewrite-rep1.json",
        "kb/selective_patch-rep1.json",
        "python/no_update-rep1.jsonl",
        "python/full_rewrite-rep1.jsonl",
        "python/selective_patch-rep1.jsonl",
    ] {
        assert!(output_dir.join(file).is_file(), "missing {file}");
    }
    // Every event line is schema-valid (checked with the library's own reader).
    let events = harness_core::read_events(output_dir.join("events.jsonl")).expect("valid events");
    assert!(events.len() > 30);
    let summary: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(output_dir.join("summary.json")).unwrap())
            .unwrap();
    assert_eq!(summary["status"], "ok");
    assert_eq!(summary["cost"]["priced_usd"], serde_json::Value::Null);
    assert_eq!(summary["cost"]["local_dispatch_cap_usd"], 100.0);
}

#[test]
fn run_refuses_an_existing_run_unless_forced() {
    let dir = tempfile::tempdir().unwrap();
    let output_dir = dir.path().join("run");
    let config = smoke();
    let args = |force: bool| -> Vec<std::ffi::OsString> {
        let mut a: Vec<std::ffi::OsString> = vec![
            "run".into(),
            "--config".into(),
            config.clone().into_os_string(),
            "--output".into(),
            output_dir.clone().into_os_string(),
        ];
        if force {
            a.push("--force".into());
        }
        a
    };
    let run_with = |force: bool| {
        Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args(force))
            .output()
            .expect("run harness")
    };
    assert_eq!(run_with(false).status.code(), Some(0));
    let again = run_with(false);
    assert_eq!(again.status.code(), Some(1));
    assert!(
        err(&again).contains("already holds a run"),
        "{}",
        err(&again)
    );
    assert_eq!(run_with(true).status.code(), Some(0));
}

#[test]
fn run_with_an_invalid_config_writes_nothing_and_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.toml");
    let text = fs::read_to_string(smoke()).unwrap().replace(
        "ids = [\"tracker-create-high-priority\"]",
        "ids = [\"ghost\"]",
    );
    // The config file lives elsewhere: keep its relative paths valid.
    let text = text
        .replace(
            "root = \"smoke\"",
            &format!("root = \"{}/fixtures/smoke\"", repo_root().display()),
        )
        .replace(
            "mock_responses_dir = \"smoke/provider\"",
            &format!(
                "mock_responses_dir = \"{}/fixtures/smoke/provider\"",
                repo_root().display()
            ),
        )
        .replace(
            "fixture_dir = \"smoke/runner\"",
            &format!(
                "fixture_dir = \"{}/fixtures/smoke/runner\"",
                repo_root().display()
            ),
        );
    fs::write(&bad, text).unwrap();
    let output_dir = dir.path().join("out");
    let output = harness(&[
        s("run"),
        s("--config"),
        bad.as_os_str(),
        s("--output"),
        output_dir.as_os_str(),
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert!(err(&output).contains("ghost"), "{}", err(&output));
    assert!(!output_dir.exists());
}

/// A paid-provider config pointing at a closed local port with a tiny cap: the dispatch cap
/// refuses the first model call before anything is sent, so no network is touched.
#[test]
fn run_stopped_by_the_budget_exits_with_code_five() {
    let dir = tempfile::tempdir().unwrap();
    let root = repo_root();
    let config = dir.path().join("budget.toml");
    let fixtures = root.join("fixtures/smoke");
    fs::write(
        &config,
        format!(
            r#"
[run]
name = "budget"
seed = 1
repetitions = 1
strategies = ["no_update", "full_rewrite"]

[inputs]
adapter = "fixture"
root = "{f}"

[versions]
source = "early"
target = "late"

[tasks]
ids = ["tracker-create-high-priority"]

[provider]
kind = "openai"
model = "gpt-x"
api_key_env = "HARNESS_CLI_TEST_KEY"
base_url = "https://127.0.0.1:9/v1"
max_output_tokens = 100

[judge]
version = "judge-x"

[budget]
cap_usd = 0.000000001

[budget.rate_card]
id = "test-rates"
date = "2026-01-01"

[[budget.rate_card.entries]]
provider = "openai"
model = "gpt-x"
input_usd_per_mtok = 1000.0
output_usd_per_mtok = 1000.0

[runner]
kind = "fixture"
fixture_dir = "{f}/runner"
"#,
            f = fixtures.display()
        ),
    )
    .unwrap();
    let output_dir = dir.path().join("out");
    let output = Command::new(env!("CARGO_BIN_EXE_harness"))
        .args([
            s("run"),
            s("--config"),
            config.as_os_str(),
            s("--output"),
            output_dir.as_os_str(),
        ])
        .env("HARNESS_CLI_TEST_KEY", "sk-not-a-real-key-000000000000")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(5),
        "{}\n{}",
        out(&output),
        err(&output)
    );
    assert!(
        out(&output).contains("budget_exhausted"),
        "{}",
        out(&output)
    );
    let all = fs::read_to_string(output_dir.join("events.jsonl")).unwrap();
    assert!(!all.contains("sk-not-a-real-key"));
}

// -------------------------------------------------------------------------------------------
// `mcpe`
// -------------------------------------------------------------------------------------------

use harness_cli::launcher::{Lookup, McpeArgs, resolve_mcpe};

fn mcpe_cmd(args: &[&std::ffi::OsStr], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mcpe"))
        .args(args)
        .current_dir(cwd)
        .env_remove("OPENAI_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("MCPE_MODEL")
        .env_remove("MCPE_THEME")
        .output()
        .expect("run mcpe")
}

#[test]
fn mcpe_help_lists_every_flag_the_defaults_and_examples() {
    let dir = tempfile::tempdir().unwrap();
    let output = mcpe_cmd(&[s("--help")], dir.path());
    assert!(output.status.success(), "{}", err(&output));
    let text = out(&output);
    for flag in [
        "-m, --model",
        "-e, --effort",
        "-w, --workspace",
        "--mcp-config",
        "--kb",
        "--sessions-dir",
        "--budget-usd",
        "--models",
        "--mock-script",
        "--theme",
        "--config",
        "-V, --version",
        "-h, --help",
    ] {
        assert!(text.contains(flag), "{flag} missing from:\n{text}");
    }
    for example in [
        "mcpe -m flash -e high",
        "mcpe --mcp-config ./servers.json",
        "Examples:",
    ] {
        assert!(text.contains(example), "{example} missing from:\n{text}");
    }
    assert!(text.contains("MCPE_MODEL"), "{text}");
    assert!(text.contains(".mcpe/sessions"), "{text}");
}

#[test]
fn mcpe_reports_its_version() {
    let dir = tempfile::tempdir().unwrap();
    let output = mcpe_cmd(&[s("-V")], dir.path());
    assert!(output.status.success());
    assert!(out(&output).starts_with("mcpe "), "{}", out(&output));
}

#[test]
fn mcpe_takes_no_required_arguments_and_rejects_unknown_ones_with_usage_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let output = mcpe_cmd(&[s("--nope")], dir.path());
    assert_eq!(output.status.code(), Some(2));
    assert!(err(&output).contains("--nope"), "{}", err(&output));
}

#[test]
fn mcpe_without_a_terminal_fails_cleanly_from_any_directory_without_a_registry_flag() {
    let dir = tempfile::tempdir().unwrap();
    let output = mcpe_cmd(
        &[
            s("--mock-script"),
            repo_root().join("fixtures/agent/hello").as_os_str(),
            s("--workspace"),
            dir.path().as_os_str(),
        ],
        dir.path(),
    );
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    let stderr = err(&output);
    assert!(
        stderr.starts_with("error: cannot initialize the terminal"),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(
        out(&output).is_empty(),
        "no escape codes on stdout: {:?}",
        out(&output)
    );
    // The embedded registry was used: the temp directory has no configs/models.toml.
    assert!(!dir.path().join("configs").exists());
}

#[test]
fn harness_tui_also_uses_the_embedded_registry_and_fails_cleanly_without_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["tui", "--mock-script"])
        .arg(repo_root().join("fixtures/agent/hello"))
        .arg("--workspace")
        .arg(dir.path())
        .current_dir(dir.path())
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("run harness");
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert!(
        err(&output).starts_with("error: cannot initialize the terminal"),
        "{}",
        err(&output)
    );
    assert!(out(&output).is_empty());
}

#[test]
fn mcpe_without_any_api_key_names_every_variable_to_set() {
    let dir = tempfile::tempdir().unwrap();
    let output = mcpe_cmd(&[s("--workspace"), dir.path().as_os_str()], dir.path());
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        err(&output).trim(),
        "error: no API key found: set OPENAI_API_KEY (luna, sol, astra) or GEMINI_API_KEY (flash, pro)"
    );
    // An explicit model keeps the precise single-variable message.
    let output = mcpe_cmd(
        &[s("-m"), s("flash"), s("-w"), dir.path().as_os_str()],
        dir.path(),
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        err(&output).trim(),
        "error: environment variable GEMINI_API_KEY is not set",
        "the long hint is only for the automatic choice"
    );
}

#[test]
fn mcpe_rejects_a_bad_effort_before_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let effort = mcpe_cmd(
        &[
            s("-m"),
            s("flash"),
            s("-e"),
            s("max"),
            s("-w"),
            dir.path().as_os_str(),
            s("--mock-script"),
            repo_root().join("fixtures/agent/hello").as_os_str(),
        ],
        dir.path(),
    );
    assert_eq!(effort.status.code(), Some(1));
    assert!(
        err(&effort).contains("allowed: low, medium, high"),
        "{}",
        err(&effort)
    );
}

fn real_lookup<'a>(
    var: &'a dyn Fn(&str) -> Option<String>,
    is_file: &'a dyn Fn(&Path) -> bool,
    cwd: &'a Path,
) -> Lookup<'a> {
    Lookup {
        var,
        is_file,
        cwd,
        home: None,
    }
}

#[test]
fn the_launcher_builds_a_session_with_discovered_files_and_a_key_picked_model() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".mcp.json"),
        r#"{"mcpServers": {"calc": {"command": "true"}}}"#,
    )
    .unwrap();
    let registry = harness_cli::commands::load_registry(None).unwrap();
    let var = |name: &str| (name == "GEMINI_API_KEY").then(|| "dummy".to_owned());
    let is_file = |path: &Path| path.is_file();
    let options = resolve_mcpe(
        McpeArgs {
            mock_script: Some(repo_root().join("fixtures/agent/hello")),
            ..McpeArgs::default()
        },
        &registry,
        &real_lookup(&var, &is_file, dir.path()),
    );
    assert_eq!(options.model, "flash", "only the Gemini key is set");
    let mut session = build_tui_session(&options, &var).expect("session builds");
    assert_eq!(session.app.status.model_alias, "flash");
    let servers: Vec<&str> = session.app.mcp.keys().map(String::as_str).collect();
    assert_eq!(servers, ["calc"], "the workspace .mcp.json was found");
    assert!(
        session
            .session_dir
            .starts_with(dir.path().join(".mcpe").join("sessions")),
        "{}",
        session.session_dir.display()
    );
    assert_eq!(
        session.app.workspace,
        dir.path().display().to_string(),
        "the workspace is the current directory"
    );
    use harness_tui::AgentPort;
    assert!(session.agent.shutdown(std::time::Duration::from_secs(5)));
}

#[test]
fn the_missing_key_hint_reaches_the_session_builder() {
    let dir = tempfile::tempdir().unwrap();
    let registry = harness_cli::commands::load_registry(None).unwrap();
    let var = |_: &str| None;
    let is_file = |path: &Path| path.is_file();
    let options = resolve_mcpe(
        McpeArgs::default(),
        &registry,
        &real_lookup(&var, &is_file, dir.path()),
    );
    let Err(message) = build_tui_session(&options, &var) else {
        panic!("no key, no session");
    };
    assert_eq!(
        message,
        "no API key found: set OPENAI_API_KEY (luna, sol, astra) or GEMINI_API_KEY (flash, pro)"
    );
}
