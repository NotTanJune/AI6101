//! TOML configuration loading and validation.

use std::fs;
use std::path::Path;

use harness_core::{Config, ConfigError, ConfigProviderKind, ProviderKind, Strategy};

/// A valid mock-provider config whose paths point into `dir` (which gets created contents).
fn mock_toml() -> String {
    r#"
[run]
name = "smoke"
seed = 42
repetitions = 2
strategies = ["no_update", "full_rewrite", "selective_patch"]

[inputs]
adapter = "fixture"
root = "data"

[versions]
source = "early"
target = "late"
stages = { early = "v1", middle = "v2", late = "v3" }

[tasks]
ids = ["task-001", "task-002"]

[provider]
kind = "mock"
model = "mock-model"
temperature = 0.0
max_output_tokens = 1024

[judge]
version = "judge-v1"

[budget]
cap_usd = 100.0

[runner]
kind = "fixture"
fixture_dir = "runner"
"#
    .to_owned()
}

fn paid_toml(extra_provider: &str, rate_card: &str) -> String {
    format!(
        r#"
[run]
name = "paid"
seed = 1
repetitions = 1
strategies = ["selective_patch"]

[inputs]
adapter = "fixture"
root = "data"

[versions]
source = "early"
target = "late"

[tasks]
ids = ["t1"]

[provider]
kind = "openai"
model = "gpt-x"
api_key_env = "OPENAI_API_KEY"
max_output_tokens = 512
{extra_provider}

[judge]
version = "judge-v1"

[budget]
cap_usd = 100.0
{rate_card}

[runner]
kind = "process"
executable = "python3"
fixed_args = ["-m", "runner"]
"#
    )
}

const RATE_CARD: &str = r#"
[budget.rate_card]
id = "rc-2026-09"
date = "2026-09-01"

[[budget.rate_card.entries]]
provider = "openai"
model = "gpt-x"
input_usd_per_mtok = 2.0
output_usd_per_mtok = 8.0
"#;

/// Parse `text` with directories `data` and `runner` present under a fresh temp dir.
fn parse(text: &str) -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("data")).unwrap();
    fs::create_dir(dir.path().join("runner")).unwrap();
    let config = Config::from_toml_str(text, dir.path()).expect("parses");
    (dir, config)
}

fn problems_of(text: &str) -> Vec<String> {
    let (_dir, config) = parse(text);
    config.validate().iter().map(ToString::to_string).collect()
}

fn assert_problem(problems: &[String], needle: &str) {
    assert!(
        problems.iter().any(|p| p.contains(needle)),
        "expected a problem containing {needle:?}, got {problems:#?}"
    );
}

#[test]
fn valid_mock_config_has_no_problems_and_resolves_paths() {
    let (dir, config) = parse(&mock_toml());
    assert_eq!(config.validate(), vec![]);
    assert_eq!(config.run.repetitions, 2);
    assert_eq!(config.run.strategies, Strategy::ALL.to_vec());
    assert_eq!(config.provider.kind, ConfigProviderKind::Mock);
    assert_eq!(config.provider.kind.cost_kind(), ProviderKind::Mock);
    assert_eq!(
        config.inputs.root.as_deref(),
        Some(dir.path().join("data").as_path())
    );
    assert_eq!(
        config.runner.fixture_dir.as_deref(),
        Some(dir.path().join("runner").as_path())
    );
    assert_eq!(config.inputs.catalog_env, "MCP_DATA_DIR");
    assert_eq!(config.inputs.eval_env, "EVAL_DATA_DIR");
    assert_eq!(
        config.versions.stages.as_ref().unwrap().middle.as_deref(),
        Some("v2")
    );
}

#[test]
fn load_reads_a_file_and_load_validated_accepts_it() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("data")).unwrap();
    fs::create_dir(dir.path().join("runner")).unwrap();
    let path = dir.path().join("exp.toml");
    fs::write(&path, mock_toml()).unwrap();
    let config = Config::load_validated(&path).expect("valid");
    assert_eq!(config.base_dir, dir.path());
    assert_eq!(config.tasks.ids.len(), 2);
}

#[test]
fn load_reports_missing_file_and_bad_toml() {
    let dir = tempfile::tempdir().unwrap();
    let missing = Config::load(dir.path().join("nope.toml")).expect_err("missing");
    assert!(matches!(missing, ConfigError::Io { .. }));

    let bad = dir.path().join("bad.toml");
    fs::write(&bad, "[run\nname=").unwrap();
    assert!(matches!(Config::load(&bad), Err(ConfigError::Parse { .. })));
}

#[test]
fn unknown_strategy_fails_to_load() {
    let text = mock_toml().replace("\"no_update\"", "\"telepathy\"");
    let dir = tempfile::tempdir().unwrap();
    let err = Config::from_toml_str(&text, dir.path()).expect_err("unknown strategy");
    assert!(err.contains("telepathy"), "{err}");
}

#[test]
fn unknown_keys_fail_to_load() {
    let text = mock_toml().replace("[judge]", "[judge]\nmodel = \"x\"");
    let dir = tempfile::tempdir().unwrap();
    let err = Config::from_toml_str(&text, dir.path()).expect_err("unknown key");
    assert!(err.contains("model"), "{err}");
}

#[test]
fn zero_repetitions_is_a_problem() {
    let problems = problems_of(&mock_toml().replace("repetitions = 2", "repetitions = 0"));
    assert_problem(&problems, "run.repetitions");
}

#[test]
fn empty_and_duplicate_lists_are_problems() {
    let problems = problems_of(&mock_toml().replace(
        "strategies = [\"no_update\", \"full_rewrite\", \"selective_patch\"]",
        "strategies = []",
    ));
    assert_problem(&problems, "run.strategies");

    let problems = problems_of(&mock_toml().replace(
        "\"no_update\", \"full_rewrite\"",
        "\"no_update\", \"no_update\"",
    ));
    assert_problem(&problems, "duplicates");

    let problems = problems_of(&mock_toml().replace("[\"task-001\", \"task-002\"]", "[]"));
    assert_problem(&problems, "tasks.ids");
}

#[test]
fn identical_source_and_target_versions_are_a_problem() {
    let problems = problems_of(&mock_toml().replace("target = \"late\"", "target = \"early\""));
    assert_problem(&problems, "versions.target");

    let baseline = mock_toml().replace("target = \"late\"", "target = \"early\"\nbaseline = true");
    let problems = problems_of(&baseline);
    assert!(
        !problems.iter().any(|p| p.contains("versions.target")),
        "{problems:?}"
    );
}

#[test]
fn missing_paths_are_problems_and_mcpevol_is_accepted_by_pure_validation() {
    let dir = tempfile::tempdir().unwrap();
    // No `data` / `runner` directories exist here.
    let config = Config::from_toml_str(&mock_toml(), dir.path()).unwrap();
    let problems: Vec<_> = config.validate().iter().map(ToString::to_string).collect();
    assert_problem(&problems, "inputs.root");
    assert_problem(&problems, "runner.fixture_dir");

    // The mcpevol adapter needs no `inputs.root`; its env vars and directories are checked where
    // the adapter is built (see tests/mcpevol.rs), not by this pure validation.
    let problems =
        problems_of(&mock_toml().replace("adapter = \"fixture\"", "adapter = \"mcpevol\""));
    assert!(
        !problems.iter().any(|p| p.contains("inputs.")),
        "unexpected inputs problem: {problems:?}"
    );
}

#[test]
fn paid_provider_without_rate_card_is_a_problem() {
    let problems = problems_of(&paid_toml("", ""));
    assert_problem(&problems, "budget.rate_card");
    assert_problem(&problems, "no rate entry");

    // Rate card present but for another model.
    let other = RATE_CARD.replace("gpt-x", "gpt-y");
    assert_problem(&problems_of(&paid_toml("", &other)), "no rate entry");

    // With the right entry the config is valid.
    assert_eq!(problems_of(&paid_toml("", RATE_CARD)), Vec::<String>::new());
}

#[test]
fn paid_provider_needs_key_env_and_output_limit() {
    let text = paid_toml("", RATE_CARD)
        .replace("api_key_env = \"OPENAI_API_KEY\"\n", "")
        .replace("max_output_tokens = 512\n", "");
    let problems = problems_of(&text);
    assert_problem(&problems, "provider.api_key_env");
    assert_problem(&problems, "provider.max_output_tokens");
}

#[test]
fn secret_looking_values_in_env_fields_are_rejected() {
    for secret in [
        "sk-abcdefghijklmnopqrstuvwxyz012345",
        "sk-proj-Abc123_def456-ghi789JKL",
        "AIzaSyA1234567890abcdefghijklmnopqrstuv",
    ] {
        let text = paid_toml("", RATE_CARD).replace("OPENAI_API_KEY", secret);
        let problems = problems_of(&text);
        assert_problem(&problems, "provider.api_key_env");
        assert_problem(&problems, "looks like a secret");
    }

    // Not a secret shape, but not an env var name either.
    let text = paid_toml("", RATE_CARD).replace("OPENAI_API_KEY", "not a name!");
    assert_problem(&problems_of(&text), "environment variable name");

    // The same rule covers the inputs env fields.
    let text = mock_toml().replace(
        "root = \"data\"",
        "root = \"data\"\ncatalog_env = \"sk-abcdefghijklmnopqrstuvwxyz012345\"",
    );
    assert_problem(&problems_of(&text), "inputs.catalog_env");
}

#[test]
fn secrets_in_argv_and_urls_are_rejected() {
    let text = paid_toml("", RATE_CARD).replace(
        "fixed_args = [\"-m\", \"runner\"]",
        "fixed_args = [\"--key=sk-abcdefghijklmnopqrstuvwxyz012345\"]",
    );
    assert_problem(&problems_of(&text), "runner.fixed_args");

    let text = mock_toml().replace(
        "kind = \"mock\"",
        "kind = \"local\"\nbase_url = \"http://user:pass@localhost:8080/v1\"",
    );
    assert_problem(&problems_of(&text), "must not embed credentials");
}

#[test]
fn local_provider_needs_base_url() {
    let text = mock_toml().replace("kind = \"mock\"", "kind = \"local\"");
    assert_problem(&problems_of(&text), "provider.base_url");
    let text = mock_toml().replace(
        "kind = \"mock\"",
        "kind = \"local\"\nbase_url = \"http://localhost:8080/v1\"",
    );
    assert_eq!(problems_of(&text), Vec::<String>::new());
}

#[test]
fn numeric_ranges_are_checked() {
    let text = mock_toml().replace("temperature = 0.0", "temperature = 3.5\ntop_p = 0.0");
    let problems = problems_of(&text);
    assert_problem(&problems, "provider.temperature");
    assert_problem(&problems, "provider.top_p");

    let text = mock_toml().replace("cap_usd = 100.0", "cap_usd = 0.0");
    assert_problem(&problems_of(&text), "budget.cap_usd");

    let text = mock_toml().replace("max_output_tokens = 1024", "max_output_tokens = 0");
    assert_problem(&problems_of(&text), "provider.max_output_tokens");
}

#[test]
fn rate_card_entries_are_validated() {
    let bad = RATE_CARD
        .replace("2026-09-01", "September")
        .replace("input_usd_per_mtok = 2.0", "input_usd_per_mtok = -1.0");
    let problems = problems_of(&paid_toml("", &bad));
    assert_problem(&problems, "budget.rate_card.date");
    assert_problem(&problems, "input_usd_per_mtok");

    let dup = format!(
        "{RATE_CARD}\n[[budget.rate_card.entries]]\nprovider = \"openai\"\nmodel = \"gpt-x\"\ninput_usd_per_mtok = 1.0\noutput_usd_per_mtok = 1.0\n"
    );
    assert_problem(&problems_of(&paid_toml("", &dup)), "duplicate");
}

#[test]
fn process_runner_needs_an_executable() {
    let text = paid_toml("", RATE_CARD).replace("executable = \"python3\"", "");
    assert_problem(&problems_of(&text), "runner.executable");
}

#[test]
fn relative_executable_with_separator_is_resolved_but_bare_names_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let text = paid_toml("", RATE_CARD).replace("\"python3\"", "\"bin/runner\"");
    let config = Config::from_toml_str(&text, dir.path()).unwrap();
    let expected = dir.path().join("bin/runner");
    assert_eq!(
        config.runner.executable.as_deref(),
        Some(expected.to_str().unwrap())
    );

    let bare = Config::from_toml_str(&paid_toml("", RATE_CARD), Path::new("/x")).unwrap();
    assert_eq!(bare.runner.executable.as_deref(), Some("python3"));
}

#[test]
fn provider_kind_maps_to_cost_kind_and_name() {
    assert_eq!(ConfigProviderKind::OpenAi.name(), "openai");
    assert_eq!(ConfigProviderKind::Gemini.cost_kind(), ProviderKind::Paid);
    assert_eq!(ConfigProviderKind::Local.cost_kind(), ProviderKind::Local);
    assert_eq!(Strategy::SelectivePatch.to_string(), "selective_patch");
}

#[test]
fn config_serializes_back_to_toml_without_secrets() {
    let (_dir, config) = parse(&paid_toml("", RATE_CARD));
    let text = toml::to_string(&config).expect("serializable");
    assert!(text.contains("api_key_env = \"OPENAI_API_KEY\""));
    let back = Config::from_toml_str(&text, &config.base_dir).unwrap();
    assert_eq!(back.provider, config.provider);
    assert_eq!(back.budget, config.budget);
}

#[test]
fn mock_responses_dir_is_optional_resolved_and_checked_only_for_mock() {
    // Absent: fine, and stays None.
    let (_dir, config) = parse(&mock_toml());
    assert_eq!(config.provider.mock_responses_dir, None);
    assert_eq!(config.validate(), vec![]);

    // Present and existing: resolved against the config dir.
    let with_dir = mock_toml().replace(
        "model = \"mock-model\"",
        "model = \"mock-model\"\nmock_responses_dir = \"canned\"",
    );
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("data")).unwrap();
    fs::create_dir(dir.path().join("runner")).unwrap();
    let config = Config::from_toml_str(&with_dir, dir.path()).expect("parses");
    assert_eq!(
        config.provider.mock_responses_dir.as_deref(),
        Some(dir.path().join("canned").as_path())
    );
    let problems: Vec<String> = config.validate().iter().map(ToString::to_string).collect();
    assert_problem(&problems, "provider.mock_responses_dir");

    fs::create_dir(dir.path().join("canned")).unwrap();
    assert_eq!(config.validate(), vec![]);

    // Not validated for other kinds.
    let mut local = config.clone();
    local.provider.kind = ConfigProviderKind::Local;
    local.provider.base_url = Some("http://localhost:1/v1".to_owned());
    fs::remove_dir(dir.path().join("canned")).unwrap();
    assert_eq!(local.validate(), vec![]);
}
