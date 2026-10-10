//! Conversion from registry prices to the core cost ledger's rate card.

use harness_core::cost::{RateCard, RateEntry};

use crate::registry::{ModelRegistry, ModelSpec};

/// Rate card id used for cards built from the model registry.
pub const RATE_CARD_ID: &str = "model-registry";

/// Rate entry for one model. The entry's provider and model match the strings the agent
/// passes in its reservation requests (`provider.as_str()` and `model_id`).
pub fn rate_entry(spec: &ModelSpec) -> RateEntry {
    let p = &spec.price;
    RateEntry {
        provider: spec.provider.as_str().to_owned(),
        model: spec.model_id.clone(),
        input_usd_per_mtok: p.input_usd_per_mtok,
        output_usd_per_mtok: p.output_usd_per_mtok,
        cached_input_usd_per_mtok: p.cached_input_usd_per_mtok,
        cache_write_usd_per_mtok: p.cache_write_usd_per_mtok,
        long_context_threshold_tokens: p.long_context_threshold_tokens,
        long_input_usd_per_mtok: p.long_input_usd_per_mtok,
        long_cached_input_usd_per_mtok: p.long_cached_input_usd_per_mtok,
        long_cache_write_usd_per_mtok: p.long_cache_write_usd_per_mtok,
        long_output_usd_per_mtok: p.long_output_usd_per_mtok,
        price_date: p.price_date.clone(),
    }
}

/// Rate card covering every model in the registry. The card date is the latest `price_date`
/// of its entries (or `unknown` when none is set).
pub fn rate_card(registry: &ModelRegistry) -> RateCard {
    let entries: Vec<RateEntry> = registry.models().iter().map(rate_entry).collect();
    let date = entries
        .iter()
        .filter_map(|e| e.price_date.clone())
        .max()
        .unwrap_or_else(|| "unknown".to_owned());
    RateCard {
        id: RATE_CARD_ID.to_owned(),
        date,
        entries,
    }
}
