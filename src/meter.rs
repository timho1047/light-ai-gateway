//! Metering: cost estimation, price learning, and TPS/TTFT math.
//!
//! FILE OWNER: teammate meter-dev.
//!
//! The local ledger is the primary source of truth for spend between probes.
//! The gateway sees every request's real usage, so threshold decisions must be
//! driven per-request rather than by polling frequency.
//!
//! ## The bias rule
//!
//! Every unknown in this module is resolved in the direction of OVER-estimating
//! cost. Under-estimating makes the hard gate too permissive and spends real
//! money; over-estimating only costs a little missed throughput.
//!
//! Concretely: `PriceTable::estimate` returns None -- never Decimal::ZERO --
//! for a model it does not know, and `conservative_estimate` is the explicit
//! fallback for that case.

use crate::config::ModelPriceConfig;
use crate::domain::TokenUsage;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

/// Fallback prompt price in USD per million tokens, used when a model has no
/// learned or configured price. Deliberately high: being too pessimistic is the
/// cheap way to be wrong.
pub const CONSERVATIVE_PROMPT_PER_MILLION: u64 = 15;
/// Fallback completion price in USD per million tokens. See
/// `CONSERVATIVE_PROMPT_PER_MILLION`.
pub const CONSERVATIVE_COMPLETION_PER_MILLION: u64 = 75;

/// ASCII is roughly 4 characters per token; the division is rounded UP.
const CHARS_PER_TOKEN: u64 = 4;
/// Chat-format overhead charged per message.
const PER_MESSAGE_OVERHEAD: u64 = 4;
/// Chat-format overhead charged per request.
const REQUEST_PRIMING: u64 = 3;

/// Effective price of one model, in USD per token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub prompt_per_token: Decimal,
    pub completion_per_token: Decimal,
    /// Cached prompt tokens are typically billed at a fraction of the
    /// uncached rate. 0.1 matches the major providers.
    pub cached_prompt_discount: Decimal,
    /// How many observations this estimate is based on.
    pub samples: u64,
}

impl ModelPrice {
    pub fn is_usable(&self) -> bool {
        self.samples > 0
    }

    /// Estimated cost of one usage sample, before the account multiplier.
    ///
    /// `cached_tokens` is a SUBSET of `prompt_tokens` in the OpenAI dialect,
    /// so the uncached part is `prompt_tokens - cached_tokens`.
    /// `reasoning_tokens` is not added separately: OpenAI already counts it
    /// inside `completion_tokens`, and adding it again would over-bill.
    pub fn base_cost(&self, usage: &TokenUsage) -> Decimal {
        let cached = usage.cached_tokens.min(usage.prompt_tokens);
        let uncached = usage.prompt_tokens - cached;

        let prompt = sat_mul(Decimal::from(uncached), self.prompt_per_token);
        let cached_prompt = sat_mul(
            sat_mul(Decimal::from(cached), self.prompt_per_token),
            self.cached_prompt_discount,
        );
        let completion = sat_mul(Decimal::from(usage.completion_tokens), self.completion_per_token);

        sat_add(prompt, sat_add(cached_prompt, completion))
    }

    /// `base_cost` scaled by the account's credit multiplier.
    pub fn cost(&self, usage: &TokenUsage, multiplier: Decimal) -> Decimal {
        sat_mul(self.base_cost(usage), sanitize_multiplier(multiplier))
    }
}

/// Learned price per model.
#[derive(Debug, Clone, Default)]
pub struct PriceTable {
    prices: HashMap<String, ModelPrice>,
}

impl PriceTable {
    pub fn new() -> Self {
        PriceTable::default()
    }

    /// Estimated cost of a usage sample. Returns None when the model is
    /// unknown, so callers can fall back to a deliberately conservative guess
    /// rather than assuming free.
    ///
    /// A price installed by `set` or `seed_from_config` counts as known even
    /// though it carries `samples == 0`; check `ModelPrice::is_usable` when
    /// only probe-validated rates are acceptable.
    ///
    /// Bias: never returns zero for an unknown model. Zero would let a request
    /// through the hard gate for free.
    pub fn estimate(&self, model: &str, usage: &TokenUsage, multiplier: Decimal) -> Option<Decimal> {
        let price = self.prices.get(model)?;
        Some(price.cost(usage, multiplier))
    }

    /// Learn from an observed spend delta.
    ///
    /// `cost` is the *difference* in provider-metered spend and `usage` the
    /// difference in tokens over the same interval (typically the difference
    /// between two probes), NOT a single request. Providers apply per-model
    /// multipliers and promotions, so recovering the effective rate is more
    /// robust than a hand-maintained table.
    ///
    /// One equation cannot identify two unknowns, so the learned rate is a
    /// single BLENDED rate in USD per weighted token, applied to both prompt and
    /// completion tokens. Cached prompt tokens are weighted by the previous
    /// observation's discount (0.1 by default), which is correct as long as the
    /// cached/uncached mix stays stable between probes. Repeated observations
    /// are averaged with `samples` as the weight, so one noisy interval cannot
    /// throw away a good rate. A config-seeded prior (samples == 0) is replaced
    /// outright by the first real observation.
    ///
    /// Callers must only pass real billing deltas: an interval with no traffic
    /// (weighted tokens == 0) and a negative cost are ignored. A zero cost over
    /// a non-zero token delta IS learned, at rate 0 -- that is how free models
    /// such as "laguna-s-2.1-free" end up costing nothing and never trigger a
    /// rotation.
    pub fn observe(&mut self, model: &str, cost: Decimal, usage: &TokenUsage) {
        if cost < Decimal::ZERO {
            return;
        }
        let cached = usage.cached_tokens.min(usage.prompt_tokens);
        let uncached = usage.prompt_tokens - cached;

        let previous = self.prices.get(model).copied();
        let discount = previous
            .map(|p| p.cached_prompt_discount)
            .unwrap_or_else(default_cached_discount);

        let weighted = sat_add(
            Decimal::from(uncached),
            sat_add(
                sat_mul(Decimal::from(cached), discount),
                Decimal::from(usage.completion_tokens),
            ),
        );
        if weighted <= Decimal::ZERO {
            return;
        }

        let observed = cost / weighted;
        // A rate above one dollar per token is nonsense (a million USD per
        // million tokens) and would wedge the hard gate. Ignore it rather than
        // let one bad probe poison the table.
        if observed > max_plausible_per_token() {
            return;
        }

        let prior_samples = previous.map(|p| p.samples).unwrap_or(0);
        let (prompt_per_token, completion_per_token) = match previous {
            Some(p) if prior_samples > 0 => {
                let n = Decimal::from(prior_samples);
                let weight = n + Decimal::ONE;
                (
                    (sat_add(sat_mul(p.prompt_per_token, n), observed)) / weight,
                    (sat_add(sat_mul(p.completion_per_token, n), observed)) / weight,
                )
            }
            _ => (observed, observed),
        };

        self.prices.insert(
            model.to_string(),
            ModelPrice {
                prompt_per_token,
                completion_per_token,
                cached_prompt_discount: discount,
                samples: prior_samples.saturating_add(1),
            },
        );
    }

    /// Install prices from the [prices] config table (USD per million tokens).
    /// They are priors: `samples` stays 0 until an observation replaces them.
    /// Returns how many models were installed; entries with a non-finite or
    /// negative price are skipped rather than clamped.
    pub fn seed_from_config(&mut self, prices: &BTreeMap<String, ModelPriceConfig>) -> usize {
        let mut installed = 0;
        for (model, p) in prices {
            let (Some(prompt), Some(completion)) = (
                per_token_from_million(p.prompt_per_million),
                per_token_from_million(p.completion_per_million),
            ) else {
                continue;
            };
            let discount = if p.cached_prompt_discount.is_finite()
                && (0.0..=1.0).contains(&p.cached_prompt_discount)
            {
                Decimal::try_from(p.cached_prompt_discount)
                    .unwrap_or_else(|_| default_cached_discount())
            } else {
                default_cached_discount()
            };
            self.prices.insert(
                model.clone(),
                ModelPrice {
                    prompt_per_token: prompt,
                    completion_per_token: completion,
                    cached_prompt_discount: discount,
                    samples: 0,
                },
            );
            installed += 1;
        }
        installed
    }

    pub fn set(&mut self, model: &str, price: ModelPrice) {
        self.prices.insert(model.to_string(), price);
    }

    pub fn get(&self, model: &str) -> Option<&ModelPrice> {
        self.prices.get(model)
    }

    /// Whether this model has any price at all (learned, configured, or seeded).
    pub fn is_known(&self, model: &str) -> bool {
        self.prices.contains_key(model)
    }

    pub fn known_models(&self) -> usize {
        self.prices.len()
    }
}

/// Deliberately conservative cost for a model we know nothing about.
///
/// Used to size the hard gate when `PriceTable::estimate` returns None. It
/// over-states cost on purpose: a request may be refused a little early, but it
/// can never slip through into real metered spend.
pub fn conservative_estimate(usage: &TokenUsage, multiplier: Decimal) -> Decimal {
    let cached = usage.cached_tokens.min(usage.prompt_tokens);
    let uncached = usage.prompt_tokens - cached;
    let prompt = per_token(CONSERVATIVE_PROMPT_PER_MILLION);
    let completion = per_token(CONSERVATIVE_COMPLETION_PER_MILLION);

    sat_mul(
        sat_add(
            sat_mul(Decimal::from(uncached), prompt),
            sat_add(
                sat_mul(sat_mul(Decimal::from(cached), prompt), default_cached_discount()),
                sat_mul(Decimal::from(usage.completion_tokens), completion),
            ),
        ),
        sanitize_multiplier(multiplier),
    )
}

/// Rough prompt-token count for a chat-completions request body, used only
/// when a real count is unavailable (it sizes the hard gate, nothing else).
///
/// The heuristic walks every string in the body, so it also counts message
/// roles and tool schemas. That over-count is intentional. Per string: ASCII is
/// charged at 4 characters per token (rounded up) and every non-ASCII character
/// is charged a whole token, because CJK is close to one token per character
/// and charging 4 chars/token there would under-state the gate.
pub fn estimate_prompt_tokens(body: &Value) -> u64 {
    let mut tokens = 0u64;
    count_text(body, &mut tokens);

    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        tokens = tokens.saturating_add(PER_MESSAGE_OVERHEAD.saturating_mul(messages.len() as u64));
        tokens = tokens.saturating_add(REQUEST_PRIMING);
    }
    tokens
}

/// Conservative completion estimate used to size the hard gate.
///
/// A declared `max_tokens` is honoured as-is: capping it would UNDER-size the
/// gate, and under-estimating is the one direction that spends real money.
/// Without a declared value we assume a generous output rather than a small one.
pub fn estimate_completion_tokens(max_tokens: Option<u64>, prompt_tokens: u64) -> u64 {
    match max_tokens {
        Some(m) => m,
        None => (prompt_tokens / 2).max(1024),
    }
}

/// Tokens per second over the generation window only. The time to first token
/// is deliberately excluded from the denominator.
///
/// Pass the instant the FIRST CONTENT chunk arrived as `first_content` (not
/// the request start) and the instant of the last content chunk as
/// `last_content`. Returns None when the window is empty or time ran
/// backwards.
pub fn compute_tps(
    completion_tokens: u64,
    first_content: Instant,
    last_content: Instant,
) -> Option<f64> {
    let window = last_content.checked_duration_since(first_content)?;
    let secs = window.as_secs_f64();
    if secs <= 0.0 {
        return None;
    }
    Some(completion_tokens as f64 / secs)
}

/// Extract usage from an OpenAI-shaped response or chunk.
///
/// Accepts either the full body ({"usage": {...}}) or the standalone usage
/// frame streaming sends after the last content chunk, whose `choices` array
/// is empty. Returns None when no usage counters were reported at all: a
/// payload with no usage must never be turned into a measured zero.
///
/// DeepSeek reports prompt-cache hits as `prompt_cache_hit_tokens` instead of
/// `prompt_tokens_details.cached_tokens`; both are accepted because DeepSeek
/// is one of this gateway's providers and that number drives the cost model.
pub fn usage_from_openai_json(v: &Value) -> Option<TokenUsage> {
    let usage = v.get("usage").filter(|u| u.is_object()).unwrap_or(v);
    if !usage.is_object() {
        return None;
    }

    let prompt_tokens = usage.get("prompt_tokens").and_then(Value::as_u64);
    let completion_tokens = usage.get("completion_tokens").and_then(Value::as_u64);
    let cached_tokens = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(Value::as_u64));
    let reasoning_tokens = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.get("reasoning_tokens").and_then(Value::as_u64));

    if prompt_tokens.is_none()
        && completion_tokens.is_none()
        && cached_tokens.is_none()
        && reasoning_tokens.is_none()
    {
        return None;
    }

    Some(TokenUsage {
        prompt_tokens: prompt_tokens.unwrap_or(0),
        completion_tokens: completion_tokens.unwrap_or(0),
        cached_tokens: cached_tokens.unwrap_or(0),
        reasoning_tokens: reasoning_tokens.unwrap_or(0),
        measured: true,
    })
}

/// Extract usage from an Anthropic-shaped response or event.
///
/// Handles both the `message_start` event (usage nested under `message`) and
/// `message_delta` (usage at the top level), and tolerates a bare usage object.
///
/// Dialect difference that must not be papered over: Anthropic's
/// `input_tokens` counts only the UNCACHED prompt, so
/// `cache_read_input_tokens` and `cache_creation_input_tokens` are added to
/// it. `cached_tokens` is the cache READ portion, the part that gets the
/// discount. Cache-creation tokens are charged at the plain uncached rate here
/// even though providers bill them at a premium; the residual is absorbed by
/// the self-learning path.
pub fn usage_from_anthropic_json(v: &Value) -> Option<TokenUsage> {
    let usage = v
        .get("usage")
        .or_else(|| v.pointer("/message/usage"))
        .filter(|u| u.is_object())
        .unwrap_or(v);
    if !usage.is_object() {
        return None;
    }

    let input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
    let output_tokens = usage.get("output_tokens").and_then(Value::as_u64);
    let cache_read = usage.get("cache_read_input_tokens").and_then(Value::as_u64);
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);

    if input_tokens.is_none()
        && output_tokens.is_none()
        && cache_read.is_none()
        && cache_creation.is_none()
    {
        return None;
    }

    let prompt_tokens = input_tokens
        .unwrap_or(0)
        .saturating_add(cache_read.unwrap_or(0))
        .saturating_add(cache_creation.unwrap_or(0));

    Some(TokenUsage {
        prompt_tokens,
        completion_tokens: output_tokens.unwrap_or(0),
        cached_tokens: cache_read.unwrap_or(0),
        reasoning_tokens: 0,
        measured: true,
    })
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

fn default_cached_discount() -> Decimal {
    Decimal::new(1, 1)
}

fn max_plausible_per_token() -> Decimal {
    Decimal::ONE
}

fn per_token(per_million: u64) -> Decimal {
    Decimal::from(per_million) / Decimal::from(1_000_000u64)
}

fn per_token_from_million(per_million: f64) -> Option<Decimal> {
    if !per_million.is_finite() || per_million < 0.0 {
        return None;
    }
    let per_million = Decimal::try_from(per_million).ok()?;
    Some(per_million / Decimal::from(1_000_000u64))
}

/// Multiplication that saturates instead of panicking on overflow. Saturation
/// points at Decimal::MAX, the conservative (over-estimating) direction.
fn sat_mul(a: Decimal, b: Decimal) -> Decimal {
    a.checked_mul(b).unwrap_or(Decimal::MAX)
}

/// Addition that saturates instead of panicking on overflow.
fn sat_add(a: Decimal, b: Decimal) -> Decimal {
    a.checked_add(b).unwrap_or(Decimal::MAX)
}

/// A multiplier of zero or less would make every request look free, which is
/// the dangerous direction. Treat nonsense as the neutral 1.0.
fn sanitize_multiplier(multiplier: Decimal) -> Decimal {
    if multiplier > Decimal::ZERO {
        multiplier
    } else {
        Decimal::ONE
    }
}

fn count_text(v: &Value, out: &mut u64) {
    match v {
        Value::String(s) => *out = out.saturating_add(text_tokens(s)),
        Value::Array(items) => items.iter().for_each(|i| count_text(i, out)),
        Value::Object(map) => map.values().for_each(|x| count_text(x, out)),
        _ => {}
    }
}

fn text_tokens(s: &str) -> u64 {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for ch in s.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii.div_ceil(CHARS_PER_TOKEN).saturating_add(non_ascii)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn price(prompt: i64, completion: i64, discount_scale: u32) -> ModelPrice {
        ModelPrice {
            prompt_per_token: Decimal::new(prompt, 6),
            completion_per_token: Decimal::new(completion, 6),
            cached_prompt_discount: Decimal::new(1, discount_scale),
            samples: 1,
        }
    }

    fn usage(prompt: u64, completion: u64, cached: u64) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: cached,
            reasoning_tokens: 0,
            measured: true,
        }
    }

    #[test]
    fn cached_prompt_token_costs_less_than_an_uncached_one() {
        let mut table = PriceTable::new();
        table.set("m", price(3, 15, 1)); // $3 / $15 per million

        let uncached = table
            .estimate("m", &usage(1_000, 0, 0), Decimal::ONE)
            .unwrap();
        let cached = table
            .estimate("m", &usage(1_000, 0, 1_000), Decimal::ONE)
            .unwrap();

        assert!(
            cached < uncached,
            "cached {cached} must be less than uncached {uncached}"
        );
        // 1000 * 0.000003 = 0.003 ; 1000 * 0.000003 * 0.1 = 0.0003
        assert_eq!(uncached, Decimal::new(3, 3));
        assert_eq!(cached, Decimal::new(3, 4));
    }

    #[test]
    fn completion_tokens_are_priced_at_the_completion_rate() {
        let mut table = PriceTable::new();
        table.set("m", price(3, 15, 1));
        let cost = table
            .estimate("m", &usage(0, 1_000, 0), Decimal::ONE)
            .unwrap();
        assert_eq!(cost, Decimal::new(15, 3)); // 1000 * 0.000015
    }

    #[test]
    fn credit_multiplier_is_applied_to_the_whole_cost() {
        let mut table = PriceTable::new();
        table.set("m", price(3, 15, 1));
        let u = usage(1_000, 1_000, 0);

        let one = table.estimate("m", &u, Decimal::ONE).unwrap();
        let four = table.estimate("m", &u, Decimal::from(4)).unwrap();
        assert_eq!(four, one * Decimal::from(4));

        // A zero or negative multiplier must not make a request look free.
        assert_eq!(table.estimate("m", &u, Decimal::ZERO).unwrap(), one);
        assert_eq!(table.estimate("m", &u, Decimal::NEGATIVE_ONE).unwrap(), one);
    }

    #[test]
    fn unknown_model_estimates_none_not_zero() {
        let table = PriceTable::new();
        let u = usage(5_000, 500, 0);
        assert_eq!(table.estimate("nobody/knows-this", &u, Decimal::ONE), None);
        assert!(!table.is_known("nobody/knows-this"));
        // ... and the explicit fallback is strictly positive, never free.
        assert!(conservative_estimate(&u, Decimal::ONE) > Decimal::ZERO);
    }

    #[test]
    fn conservative_estimate_is_above_a_plausible_real_price() {
        let mut table = PriceTable::new();
        table.set("m", price(3, 15, 1));
        let u = usage(10_000, 1_000, 0);
        let fallback = conservative_estimate(&u, Decimal::ONE);
        let real = table.estimate("m", &u, Decimal::ONE).unwrap();
        assert!(
            fallback > real,
            "the unknown-model fallback ({fallback}) must over-estimate the known price ({real})"
        );
    }

    #[test]
    fn observe_learns_the_effective_rate_from_a_delta() {
        let mut table = PriceTable::new();
        // Ten cents of delta spend over 1000 prompt + 1000 completion tokens.
        table.observe("m", Decimal::new(10, 2), &usage(1_000, 1_000, 0));

        let learned = table.get("m").unwrap();
        assert_eq!(learned.samples, 1);
        assert_eq!(learned.prompt_per_token, Decimal::new(5, 5)); // 0.10 / 2000
        assert_eq!(learned.completion_per_token, Decimal::new(5, 5));
        // Replaying the same delta through the learned rate returns the same cost.
        let cost = table
            .estimate("m", &usage(1_000, 1_000, 0), Decimal::ONE)
            .unwrap();
        assert_eq!(cost, Decimal::new(10, 2));
    }

    #[test]
    fn observe_weights_cached_tokens_by_the_discount() {
        let mut table = PriceTable::new();
        // All 1000 prompt tokens were cache reads: weight = 1000 * 0.1 = 100.
        table.observe("m", Decimal::new(1, 2), &usage(1_000, 0, 1_000));
        assert_eq!(
            table.get("m").unwrap().prompt_per_token,
            Decimal::new(1, 4) // 0.01 / 100
        );
    }

    #[test]
    fn observe_averages_repeated_observations() {
        let mut table = PriceTable::new();
        table.observe("m", Decimal::from(1), &usage(1_000, 0, 0)); // rate 0.001
        table.observe("m", Decimal::from(3), &usage(1_000, 0, 0)); // rate 0.003
        let learned = table.get("m").unwrap();
        assert_eq!(learned.samples, 2);
        assert_eq!(learned.prompt_per_token, Decimal::new(2, 3)); // mean 0.002
        assert_eq!(
            table
                .estimate("m", &usage(1_000, 0, 0), Decimal::ONE)
                .unwrap(),
            Decimal::from(2)
        );
    }

    #[test]
    fn observe_learns_zero_for_a_free_model() {
        // DESIGN section 3: free models must fall out as a zero rate so they
        // never look like they are burning quota.
        let mut table = PriceTable::new();
        table.observe("laguna-s-2.1-free", Decimal::ZERO, &usage(500, 100, 0));
        let learned = table.get("laguna-s-2.1-free").unwrap();
        assert_eq!(learned.samples, 1);
        assert_eq!(learned.prompt_per_token, Decimal::ZERO);
        assert_eq!(
            table
                .estimate("laguna-s-2.1-free", &usage(500, 100, 0), Decimal::ONE)
                .unwrap(),
            Decimal::ZERO
        );
    }

    #[test]
    fn observe_ignores_an_interval_with_no_traffic_or_negative_cost() {
        let mut table = PriceTable::new();
        table.observe("m", Decimal::from(1), &usage(0, 0, 0));
        table.observe("m", Decimal::NEGATIVE_ONE, &usage(10, 10, 0));
        assert_eq!(table.known_models(), 0);
    }

    #[test]
    fn observe_rejects_an_implausible_rate() {
        let mut table = PriceTable::new();
        table.observe("m", Decimal::from(1_000_000), &usage(1, 0, 0));
        assert_eq!(
            table.known_models(),
            0,
            "a 1M USD per token rate must not be learned"
        );
    }

    #[test]
    fn config_seeded_priors_are_used_and_replaced_by_the_first_observation() {
        let mut prices = BTreeMap::new();
        prices.insert(
            "m".to_string(),
            ModelPriceConfig {
                prompt_per_million: 3.0,
                completion_per_million: 15.0,
                cached_prompt_discount: 0.1,
            },
        );

        let mut table = PriceTable::new();
        assert_eq!(table.seed_from_config(&prices), 1);
        assert!(table.is_known("m"));
        assert!(
            !table.get("m").unwrap().is_usable(),
            "a configured prior has samples = 0"
        );
        assert_eq!(
            table.estimate("m", &usage(1_000_000, 0, 0), Decimal::ONE),
            Some(Decimal::from(3))
        );

        // The first real observation replaces the prior outright.
        table.observe("m", Decimal::new(5, 2), &usage(1_000, 0, 0));
        assert_eq!(table.get("m").unwrap().samples, 1);
        assert_eq!(table.get("m").unwrap().prompt_per_token, Decimal::new(5, 5));
    }

    #[test]
    fn config_seeded_priors_skip_broken_prices() {
        let mut prices = BTreeMap::new();
        prices.insert(
            "bad".to_string(),
            ModelPriceConfig {
                prompt_per_million: f64::NAN,
                completion_per_million: 1.0,
                cached_prompt_discount: 0.1,
            },
        );
        prices.insert(
            "negative".to_string(),
            ModelPriceConfig {
                prompt_per_million: -1.0,
                completion_per_million: 1.0,
                cached_prompt_discount: 0.1,
            },
        );
        let mut table = PriceTable::new();
        assert_eq!(table.seed_from_config(&prices), 0);
        assert_eq!(table.estimate("bad", &usage(1, 1, 0), Decimal::ONE), None);
    }

    #[test]
    fn openai_usage_reads_cache_and_reasoning_details() {
        // Shape of an OpenAI chat.completion body.
        let body = json!({
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "model": "deepseek/deepseek-v3.2",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hello" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 1200,
                "completion_tokens": 300,
                "total_tokens": 1500,
                "prompt_tokens_details": { "cached_tokens": 1024, "audio_tokens": 0 },
                "completion_tokens_details": { "reasoning_tokens": 128, "audio_tokens": 0 }
            }
        });

        let u = usage_from_openai_json(&body).expect("usage must be extracted");
        assert_eq!(u.prompt_tokens, 1200);
        assert_eq!(u.completion_tokens, 300);
        assert_eq!(u.cached_tokens, 1024);
        assert_eq!(u.reasoning_tokens, 128);
        assert!(u.measured);
    }

    #[test]
    fn openai_streaming_usage_frame_with_empty_choices_is_parsed() {
        // The usage frame arrives on its own, after finish_reason, with an
        // EMPTY choices array; a parser reading choices[0] would fail here.
        let frame = json!({
            "id": "chatcmpl-123",
            "object": "chat.completion.chunk",
            "choices": [],
            "usage": { "prompt_tokens": 9, "completion_tokens": 12, "total_tokens": 21 }
        });
        let u = usage_from_openai_json(&frame).unwrap();
        assert_eq!(u.prompt_tokens, 9);
        assert_eq!(u.completion_tokens, 12);
        assert_eq!(u.cached_tokens, 0);
        assert!(u.measured);
    }

    #[test]
    fn openai_usage_accepts_a_bare_usage_object_and_deepseek_cache_fields() {
        let bare = json!({ "prompt_tokens": 4, "completion_tokens": 5, "total_tokens": 9 });
        assert_eq!(usage_from_openai_json(&bare).unwrap().prompt_tokens, 4);

        // DeepSeek reports cache hits as prompt_cache_hit_tokens.
        let deepseek = json!({
            "usage": {
                "prompt_tokens": 9,
                "completion_tokens": 12,
                "total_tokens": 21,
                "prompt_cache_hit_tokens": 5,
                "prompt_cache_miss_tokens": 4
            }
        });
        assert_eq!(usage_from_openai_json(&deepseek).unwrap().cached_tokens, 5);
    }

    #[test]
    fn openai_payloads_without_usage_yield_none_never_a_measured_zero() {
        let content_only = json!({
            "choices": [{ "index": 0, "delta": { "content": "hi" } }]
        });
        assert!(usage_from_openai_json(&content_only).is_none());

        let empty_usage = json!({ "choices": [], "usage": {} });
        assert!(usage_from_openai_json(&empty_usage).is_none());

        let null_usage = json!({ "choices": [], "usage": null });
        assert!(usage_from_openai_json(&null_usage).is_none());
    }

    #[test]
    fn anthropic_usage_adds_cache_tokens_to_the_prompt_total() {
        // message_start nests usage under "message".
        let event = json!({
            "type": "message_start",
            "message": {
                "id": "msg_0144",
                "type": "message",
                "role": "assistant",
                "content": [],
                "usage": {
                    "input_tokens": 2095,
                    "output_tokens": 1,
                    "cache_creation_input_tokens": 500,
                    "cache_read_input_tokens": 1000
                }
            }
        });

        let u = usage_from_anthropic_json(&event).unwrap();
        assert_eq!(u.prompt_tokens, 3595); // input + cache_read + cache_creation
        assert_eq!(u.completion_tokens, 1);
        assert_eq!(u.cached_tokens, 1000);
        assert_eq!(u.reasoning_tokens, 0);
        assert!(u.measured);
    }

    #[test]
    fn anthropic_message_delta_carries_its_own_usage() {
        let event = json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": 503 }
        });
        let u = usage_from_anthropic_json(&event).unwrap();
        assert_eq!(u.completion_tokens, 503);
        assert_eq!(u.prompt_tokens, 0);
        assert!(u.measured);
    }

    #[test]
    fn anthropic_events_without_usage_yield_none() {
        assert!(usage_from_anthropic_json(&json!({ "type": "ping" })).is_none());
        assert!(
            usage_from_anthropic_json(&json!({ "type": "message_stop" })).is_none(),
            "a stop event must not be reported as a measured zero"
        );
        assert!(usage_from_anthropic_json(&json!({ "usage": {} })).is_none());
    }

    #[test]
    fn tps_excludes_ttft_from_the_denominator() {
        let request_start = Instant::now();
        let first_content = request_start + Duration::from_millis(3_000); // 3s TTFT
        let last_content = request_start + Duration::from_millis(5_000); // 2s of tokens

        let tps = compute_tps(100, first_content, last_content).unwrap();
        assert!(
            (tps - 50.0).abs() < 1e-9,
            "expected 100 tokens / 2s, got {tps}"
        );
        // Had TTFT leaked into the denominator this would be 20.0.
        assert!((tps - 20.0).abs() > 1.0);
    }

    #[test]
    fn tps_is_none_when_the_generation_window_is_empty() {
        let t = Instant::now();
        assert_eq!(compute_tps(100, t, t), None);
    }

    #[test]
    fn tps_is_none_when_time_runs_backwards() {
        let first = Instant::now();
        let last = first + Duration::from_millis(100);
        assert_eq!(compute_tps(100, last, first), None);
    }

    #[test]
    fn prompt_estimate_is_zero_for_an_empty_body_and_ascii_is_four_chars_per_token() {
        assert_eq!(estimate_prompt_tokens(&json!({})), 0);

        let body = json!({ "messages": [{ "role": "user", "content": "abcdefgh" }] });
        // "user" (4 ascii -> 1) + "abcdefgh" (8 ascii -> 2) + 4 per message + 3 priming
        assert_eq!(estimate_prompt_tokens(&body), 10);
    }

    #[test]
    fn prompt_estimate_charges_cjk_per_character() {
        let body = json!({
            "messages": [{ "role": "user", "content": "你好世界你好世界你好世界" }]
        });
        let tokens = estimate_prompt_tokens(&body);
        assert!(
            tokens >= 12,
            "12 CJK characters must not be under-counted at 4 chars/token: got {tokens}"
        );
    }

    #[test]
    fn completion_estimate_honours_a_declared_max_and_never_returns_zero() {
        assert_eq!(estimate_completion_tokens(Some(32_000), 10), 32_000);
        assert_eq!(estimate_completion_tokens(None, 0), 1024);
        assert!(estimate_completion_tokens(None, 100_000) >= 50_000);
    }

    #[test]
    fn learned_price_drives_the_full_cost_model() {
        let mut table = PriceTable::new();
        table.observe("m", Decimal::new(1, 2), &usage(2_000, 0, 0)); // 0.000005/token
        assert_eq!(
            table.get("m").unwrap().prompt_per_token,
            Decimal::new(5, 6)
        );

        let cost = table
            .estimate("m", &usage(1_000, 1_000, 1_000), Decimal::from(2))
            .unwrap();
        // (0 uncached + 1000 cached * 0.1 + 1000 completion) * 5e-6 * 2 = 0.011
        assert_eq!(cost, Decimal::new(11, 3));
    }
}
