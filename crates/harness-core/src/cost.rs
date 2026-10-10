//! Cost accounting: rate cards, budget reservations and aggregation.
//!
//! Invariants:
//! * unknown usage or price is never reported as zero (`estimated_usd` is `None`);
//! * mock and local calls never contribute USD;
//! * a paid call whose usage never came back keeps its reservation as `unreconciled_usd`, so
//!   it keeps counting against the cap.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::events::{CostStatus, ModelCallEvent, Phase};

/// A `model_call` event doubles as the cost record.
pub type CostRecord = ModelCallEvent;

/// Tolerance for floating-point comparisons against the cap.
const EPSILON_USD: f64 = 1e-12;

/// Whether a provider costs money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Paid remote API.
    Paid,
    /// Local inference: tracked by latency, never USD.
    Local,
    /// Mock provider: never USD.
    Mock,
}

/// USD price per million tokens for one provider/model pair.
///
/// The optional fields describe prompt caching and long-context pricing. When a cached or
/// cache-write rate is `None`, the plain input rate applies. When the long-context threshold
/// is set and a call's input exceeds it, the whole call is billed at the long-context rates
/// (each long rate falls back to its short counterpart when `None`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateEntry {
    /// Provider name (matches [`ReserveRequest::provider`]).
    pub provider: String,
    /// Model id.
    pub model: String,
    /// USD per million input tokens.
    pub input_usd_per_mtok: f64,
    /// USD per million output tokens.
    pub output_usd_per_mtok: f64,
    /// USD per million cached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_usd_per_mtok: Option<f64>,
    /// USD per million cache-write input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_usd_per_mtok: Option<f64>,
    /// Input token count above which the long-context rates apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context_threshold_tokens: Option<u64>,
    /// Long-context USD per million input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_input_usd_per_mtok: Option<f64>,
    /// Long-context USD per million cached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_cached_input_usd_per_mtok: Option<f64>,
    /// Long-context USD per million cache-write input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_cache_write_usd_per_mtok: Option<f64>,
    /// Long-context USD per million output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_output_usd_per_mtok: Option<f64>,
    /// ISO date (`YYYY-MM-DD`) the prices were taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_date: Option<String>,
}

/// Token counts used to price one call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TokenCounts {
    input: u64,
    cached_input: Option<u64>,
    cache_write: Option<u64>,
    output: u64,
}

impl RateEntry {
    /// Price in USD for one call.
    ///
    /// Assumption: `cached_input` and `cache_write` are subsets of `input` (the OpenAI caching
    /// guide computes hit rate as cached over input; cache writes are not stated). Output
    /// tokens include reasoning tokens. This only affects how cheap a cached call looks; it
    /// never under-reserves, because reservations bill every estimated input token at the
    /// uncached input rate.
    fn cost_usd(&self, counts: TokenCounts) -> f64 {
        let long = self
            .long_context_threshold_tokens
            .is_some_and(|threshold| counts.input > threshold);
        let (input_rate, cached_rate, write_rate, output_rate) = if long {
            let input = self
                .long_input_usd_per_mtok
                .unwrap_or(self.input_usd_per_mtok);
            (
                input,
                self.long_cached_input_usd_per_mtok
                    .or(self.cached_input_usd_per_mtok)
                    .unwrap_or(input),
                self.long_cache_write_usd_per_mtok
                    .or(self.cache_write_usd_per_mtok)
                    .unwrap_or(input),
                self.long_output_usd_per_mtok
                    .unwrap_or(self.output_usd_per_mtok),
            )
        } else {
            (
                self.input_usd_per_mtok,
                self.cached_input_usd_per_mtok
                    .unwrap_or(self.input_usd_per_mtok),
                self.cache_write_usd_per_mtok
                    .unwrap_or(self.input_usd_per_mtok),
                self.output_usd_per_mtok,
            )
        };
        let cached = counts.cached_input.unwrap_or(0);
        let writes = counts.cache_write.unwrap_or(0);
        let uncached = counts.input.saturating_sub(cached.saturating_add(writes));
        // Token counts are far below 2^53, so the conversion is exact in practice.
        (uncached as f64 * input_rate
            + cached as f64 * cached_rate
            + writes as f64 * write_rate
            + counts.output as f64 * output_rate)
            / 1e6
    }
}

/// Versioned price list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateCard {
    /// Rate card id.
    pub id: String,
    /// Date the prices were taken (`YYYY-MM-DD`).
    pub date: String,
    /// Prices.
    pub entries: Vec<RateEntry>,
}

impl RateCard {
    /// Rate card without prices (only mock/local providers can be used with it).
    pub fn empty(id: impl Into<String>, date: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            date: date.into(),
            entries: Vec::new(),
        }
    }

    /// Price for a provider/model pair.
    pub fn find(&self, provider: &str, model: &str) -> Option<&RateEntry> {
        self.entries
            .iter()
            .find(|e| e.provider == provider && e.model == model)
    }
}

/// Cost accounting failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CostError {
    /// A paid provider has no configured rate; the call is blocked.
    #[error("no rate configured for paid provider {provider:?} model {model:?}; call blocked")]
    NoRate {
        /// Provider name.
        provider: String,
        /// Model id.
        model: String,
    },
    /// The reservation would exceed the local dispatch cap.
    #[error(
        "reservation of ${requested_usd:.6} would exceed the local dispatch cap of ${cap_usd:.2} (remaining ${remaining_usd:.6})"
    )]
    CapExceeded {
        /// Amount the call would reserve.
        requested_usd: f64,
        /// Budget remaining before this reservation.
        remaining_usd: f64,
        /// Configured cap.
        cap_usd: f64,
    },
    /// An externally produced cost record is inconsistent.
    #[error("invalid cost record: {0}")]
    InvalidRecord(String),
}

/// Request to reserve budget before a call.
#[derive(Debug, Clone, PartialEq)]
pub struct ReserveRequest {
    /// Provider name.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Pipeline phase.
    pub phase: Phase,
    /// Estimated input tokens of the request.
    pub input_tokens_estimate: u64,
    /// Output token limit set on the call.
    pub max_output_tokens: u32,
    /// Provider cost kind.
    pub kind: ProviderKind,
}

/// Budget held for one in-flight call. Must be passed to [`CostLedger::settle`] or
/// [`CostLedger::release`].
#[derive(Debug)]
#[must_use = "settle or release the reservation"]
pub struct Reservation {
    id: u64,
    provider: String,
    model: String,
    phase: Phase,
    kind: ProviderKind,
    reserved_usd: f64,
    rate: Option<RateEntry>,
}

impl Reservation {
    /// USD held by this reservation (0 for mock/local).
    pub fn reserved_usd(&self) -> f64 {
        self.reserved_usd
    }

    /// Provider cost kind.
    pub fn kind(&self) -> ProviderKind {
        self.kind
    }

    /// Pipeline phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }
}

/// Usage reported by a finished call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    /// Provider request id.
    pub request_id: String,
    /// Input tokens, when the provider returned them.
    pub input_tokens: Option<u64>,
    /// Output tokens (including reasoning tokens), when the provider returned them.
    pub output_tokens: Option<u64>,
    /// Cached input tokens (a subset of `input_tokens`), when reported.
    pub cached_input_tokens: Option<u64>,
    /// Cache-write input tokens (a subset of `input_tokens`), when reported.
    pub cache_write_tokens: Option<u64>,
    /// Reasoning tokens. Informational: already included in `output_tokens`.
    pub reasoning_tokens: Option<u64>,
    /// Wall-clock latency.
    pub latency_ms: u64,
}

/// Aggregated totals for one phase.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PhaseTotals {
    /// Calls recorded.
    pub calls: u64,
    /// Input tokens where reported.
    pub input_tokens: u64,
    /// Output tokens where reported.
    pub output_tokens: u64,
    /// USD from priced calls only.
    pub usd: f64,
    /// Calls whose cost is unknown.
    pub unknown_calls: u64,
    /// Mock calls (no USD).
    pub mock_calls: u64,
    /// Total latency of local-inference calls.
    pub local_latency_ms: u64,
}

impl PhaseTotals {
    fn add(&mut self, record: &CostRecord) {
        self.calls += 1;
        self.input_tokens += record.input_tokens.unwrap_or(0);
        self.output_tokens += record.output_tokens.unwrap_or(0);
        match record.cost_status {
            CostStatus::Priced => self.usd += record.estimated_usd.unwrap_or(0.0),
            CostStatus::Unknown => self.unknown_calls += 1,
            CostStatus::Mock => self.mock_calls += 1,
            CostStatus::Local => self.local_latency_ms += record.latency_ms,
        }
    }
}

/// Tracks spend against a local dispatch cap. This is a local guard, not the provider's bill.
#[derive(Debug, Clone)]
pub struct CostLedger {
    cap_usd: f64,
    rate_card: RateCard,
    spent_usd: f64,
    unreconciled_usd: f64,
    outstanding: BTreeMap<u64, f64>,
    next_id: u64,
    records: Vec<CostRecord>,
    totals: BTreeMap<Phase, PhaseTotals>,
}

impl CostLedger {
    /// New ledger with a cap in USD and the rate card used for pricing.
    pub fn new(cap_usd: f64, rate_card: RateCard) -> Self {
        Self {
            cap_usd,
            rate_card,
            spent_usd: 0.0,
            unreconciled_usd: 0.0,
            outstanding: BTreeMap::new(),
            next_id: 0,
            records: Vec::new(),
            totals: BTreeMap::new(),
        }
    }

    /// Configured cap.
    pub fn cap_usd(&self) -> f64 {
        self.cap_usd
    }

    /// Rate card in use.
    pub fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }

    /// USD spent on priced calls (actual, by rate card).
    pub fn spent_usd(&self) -> f64 {
        self.spent_usd
    }

    /// USD held for paid calls whose usage never came back. Counts against the cap.
    pub fn unreconciled_usd(&self) -> f64 {
        self.unreconciled_usd
    }

    /// USD held by in-flight reservations.
    pub fn outstanding_reserved_usd(&self) -> f64 {
        self.outstanding.values().sum()
    }

    /// Cap minus spent, unreconciled and outstanding amounts (never negative).
    pub fn remaining_usd(&self) -> f64 {
        (self.cap_usd - self.spent_usd - self.unreconciled_usd - self.outstanding_reserved_usd())
            .max(0.0)
    }

    /// Every record settled or ingested so far, in order.
    pub fn records(&self) -> &[CostRecord] {
        &self.records
    }

    /// Totals per phase.
    pub fn totals_by_phase(&self) -> BTreeMap<Phase, PhaseTotals> {
        self.totals.clone()
    }

    /// Totals across all phases.
    pub fn totals_overall(&self) -> PhaseTotals {
        self.totals
            .values()
            .fold(PhaseTotals::default(), |mut acc, t| {
                acc.calls += t.calls;
                acc.input_tokens += t.input_tokens;
                acc.output_tokens += t.output_tokens;
                acc.usd += t.usd;
                acc.unknown_calls += t.unknown_calls;
                acc.mock_calls += t.mock_calls;
                acc.local_latency_ms += t.local_latency_ms;
                acc
            })
    }

    /// Reserve budget before a call.
    ///
    /// Paid providers need a rate entry ([`CostError::NoRate`] otherwise) and reserve
    /// `(input_estimate * in_rate + max_output * out_rate) / 1e6`, refused with
    /// [`CostError::CapExceeded`] when spent + unreconciled + outstanding + this would exceed
    /// the cap. Mock and local calls reserve nothing.
    pub fn reserve(&mut self, request: ReserveRequest) -> Result<Reservation, CostError> {
        let (reserved_usd, rate) = match request.kind {
            ProviderKind::Local | ProviderKind::Mock => (0.0, None),
            ProviderKind::Paid => {
                let rate = self
                    .rate_card
                    .find(&request.provider, &request.model)
                    .cloned()
                    .ok_or_else(|| CostError::NoRate {
                        provider: request.provider.clone(),
                        model: request.model.clone(),
                    })?;
                let amount = rate.cost_usd(TokenCounts {
                    input: request.input_tokens_estimate,
                    output: u64::from(request.max_output_tokens),
                    ..TokenCounts::default()
                });
                let remaining = self.remaining_usd();
                if amount > remaining + EPSILON_USD {
                    return Err(CostError::CapExceeded {
                        requested_usd: amount,
                        remaining_usd: remaining,
                        cap_usd: self.cap_usd,
                    });
                }
                (amount, Some(rate))
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        if reserved_usd > 0.0 {
            self.outstanding.insert(id, reserved_usd);
        }
        Ok(Reservation {
            id,
            provider: request.provider,
            model: request.model,
            phase: request.phase,
            kind: request.kind,
            reserved_usd,
            rate,
        })
    }

    /// Settle a finished call against its reservation and record it.
    pub fn settle(&mut self, reservation: Reservation, usage: Usage) -> CostRecord {
        self.outstanding.remove(&reservation.id);
        let (status, estimated_usd) = match reservation.kind {
            ProviderKind::Mock => (CostStatus::Mock, None),
            ProviderKind::Local => (CostStatus::Local, None),
            ProviderKind::Paid => {
                match (usage.input_tokens, usage.output_tokens, &reservation.rate) {
                    (Some(input), Some(output), Some(rate)) => {
                        let cost = rate.cost_usd(TokenCounts {
                            input,
                            cached_input: usage.cached_input_tokens,
                            cache_write: usage.cache_write_tokens,
                            output,
                        });
                        self.spent_usd += cost;
                        (CostStatus::Priced, Some(cost))
                    }
                    _ => {
                        // Usage missing: keep the reservation counting against the cap.
                        self.unreconciled_usd += reservation.reserved_usd;
                        (CostStatus::Unknown, None)
                    }
                }
            }
        };
        let priced_kind = reservation.kind == ProviderKind::Paid;
        let record = CostRecord {
            provider: reservation.provider,
            model: reservation.model,
            phase: reservation.phase,
            request_id: usage.request_id,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            latency_ms: usage.latency_ms,
            rate_card_id: priced_kind.then(|| self.rate_card.id.clone()),
            rate_card_date: priced_kind.then(|| self.rate_card.date.clone()),
            estimated_usd,
            cost_status: status,
        };
        self.push_record(record.clone());
        record
    }

    /// Free a reservation for a call that failed before returning usage. Nothing is recorded.
    pub fn release(&mut self, reservation: Reservation) {
        self.outstanding.remove(&reservation.id);
    }

    /// Validate and aggregate a cost record produced elsewhere (for example a Python
    /// `model_call` event). Call once per event; records are not de-duplicated.
    ///
    /// Priced records add their USD to spend; unknown, mock and local records add none.
    pub fn ingest_external(&mut self, record: &CostRecord) -> Result<(), CostError> {
        validate_record(record)?;
        if record.cost_status == CostStatus::Priced {
            self.spent_usd += record.estimated_usd.unwrap_or(0.0);
        }
        self.push_record(record.clone());
        Ok(())
    }

    fn push_record(&mut self, record: CostRecord) {
        self.totals.entry(record.phase).or_default().add(&record);
        self.records.push(record);
    }
}

fn validate_record(record: &CostRecord) -> Result<(), CostError> {
    let invalid = |msg: String| Err(CostError::InvalidRecord(msg));
    if record.provider.is_empty() || record.model.is_empty() || record.request_id.is_empty() {
        return invalid("provider, model and request_id must be non-empty".to_owned());
    }
    match (record.cost_status, record.estimated_usd) {
        (CostStatus::Priced, Some(usd)) if usd.is_finite() && usd >= 0.0 => Ok(()),
        (CostStatus::Priced, other) => invalid(format!(
            "priced record needs a finite estimated_usd >= 0, got {other:?}"
        )),
        (status, Some(usd)) => invalid(format!(
            "{status:?} record must not carry estimated_usd (got {usd}); only priced calls have USD"
        )),
        (_, None) => Ok(()),
    }
}
