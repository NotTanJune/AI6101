//! Model registry loading and validation.

use harness_agent::pricing::{rate_card, rate_entry};
use harness_agent::registry::{Effort, ModelRegistry, ProviderId, RegistryError};

const MODELS_TOML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../configs/models.toml");

fn load() -> ModelRegistry {
    ModelRegistry::load(MODELS_TOML).expect("configs/models.toml loads")
}

#[test]
fn registry_loads_shipped_registry_with_exactly_the_five_aliases() {
    let registry = load();
    assert_eq!(
        registry.aliases(),
        vec!["luna", "sol", "astra", "flash", "pro"]
    );
    assert_eq!(registry.get("luna").unwrap().model_id, "gpt-6-luna");
    assert_eq!(registry.get("sol").unwrap().model_id, "gpt-6.1-sol");
    assert_eq!(registry.get("astra").unwrap().model_id, "gpt-6-astra");
    assert_eq!(registry.get("flash").unwrap().model_id, "gemini-3.8-flash");
    assert_eq!(
        registry.get("pro").unwrap().model_id,
        "gemini-3.1-pro-preview"
    );
    assert_eq!(registry.get("sol").unwrap().provider, ProviderId::OpenAi);
    assert_eq!(registry.get("pro").unwrap().provider, ProviderId::Gemini);
    assert_eq!(
        registry.api_key_envs().into_iter().collect::<Vec<_>>(),
        vec!["GEMINI_API_KEY".to_owned(), "OPENAI_API_KEY".to_owned()]
    );
}

#[test]
fn registry_effort_rules_follow_the_facts_file() {
    let registry = load();
    assert!(!registry.get("sol").unwrap().effort_supported(Effort::None));
    assert!(
        !registry
            .get("sol")
            .unwrap()
            .effort_supported(Effort::Minimal)
    );
    assert!(registry.get("luna").unwrap().effort_supported(Effort::None));
    assert!(
        !registry
            .get("flash")
            .unwrap()
            .effort_supported(Effort::Minimal)
    );
    assert!(!registry.get("pro").unwrap().effort_supported(Effort::Max));
    assert_eq!(registry.get("luna").unwrap().default_effort, Effort::Max);
    assert_eq!(registry.get("sol").unwrap().default_effort, Effort::Xhigh);
    assert_eq!(registry.get("astra").unwrap().default_effort, Effort::High);
    assert_eq!(
        registry.get("flash").unwrap().default_effort,
        Effort::Medium
    );
    assert_eq!(registry.get("pro").unwrap().default_effort, Effort::High);
}

#[test]
fn registry_no_model_accepts_sampling_parameters() {
    let registry = load();
    assert!(!registry.get("astra").unwrap().accepts_sampling);
    assert!(registry.models().iter().all(|m| !m.accepts_sampling));
}

#[test]
fn registry_invalid_default_effort_fails_validation() {
    let text = std::fs::read_to_string(MODELS_TOML).unwrap().replacen(
        "default_effort = \"xhigh\"",
        "default_effort = \"none\"",
        1,
    );
    let err = ModelRegistry::from_toml_str(&text).expect_err("sol default none is invalid");
    assert!(
        matches!(&err, RegistryError::Invalid(m) if m.contains("sol") && m.contains("default effort")),
        "{err}"
    );
}

#[test]
fn registry_duplicate_alias_and_missing_price_fail() {
    let text = std::fs::read_to_string(MODELS_TOML).unwrap();
    let dup = text.replacen("alias = \"sol\"", "alias = \"luna\"", 1);
    let err = ModelRegistry::from_toml_str(&dup).expect_err("duplicate alias");
    assert!(
        matches!(&err, RegistryError::Invalid(m) if m.contains("duplicate")),
        "{err}"
    );

    let no_price = r#"
[[models]]
alias = "x"
provider = "openai"
model_id = "m"
api_key_env = "K"
allowed_efforts = ["low"]
default_effort = "low"
accepts_sampling = false
max_output_tokens = 10
"#;
    let err = ModelRegistry::from_toml_str(no_price).expect_err("price is required");
    assert!(matches!(err, RegistryError::Parse(_)), "{err}");
}

#[test]
fn registry_unknown_alias_is_an_error() {
    assert!(matches!(
        load().get("argon"),
        Err(RegistryError::UnknownAlias(_))
    ));
}

#[test]
fn registry_rate_card_carries_the_full_price_set() {
    let registry = load();
    let card = rate_card(&registry);
    assert_eq!(card.entries.len(), 5);
    assert_eq!(card.date, "2026-10-04");
    let sol = card.find("openai", "gpt-6.1-sol").expect("sol priced");
    assert_eq!(sol.input_usd_per_mtok, 2.0);
    assert_eq!(sol.cached_input_usd_per_mtok, Some(0.1));
    assert_eq!(sol.cache_write_usd_per_mtok, Some(2.5));
    assert_eq!(sol.long_context_threshold_tokens, Some(272_000));
    assert_eq!(sol.long_output_usd_per_mtok, Some(15.0));
    let pro = rate_entry(registry.get("pro").unwrap());
    assert_eq!(pro.provider, "gemini");
    assert_eq!(pro.long_context_threshold_tokens, Some(200_000));
    assert_eq!(pro.cache_write_usd_per_mtok, None);
    let flash = rate_entry(registry.get("flash").unwrap());
    assert_eq!(flash.long_context_threshold_tokens, None);
    assert_eq!(flash.output_usd_per_mtok, 3.75);
}
