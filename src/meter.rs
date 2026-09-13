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
use std::collections::BTreeMap;
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
    /// USD per input token.
    pub input_per_token: Decimal,
    /// USD per output token.
    pub output_per_token: Decimal,
    /// USD per cache-read input token. A price, not a ratio, so it reads the
    /// same way as the other two. Providers commonly charge a tenth of input.
    pub cache_read_per_token: Decimal,
    /// How many observations this estimate is based on.
    pub samples: u64,
}

impl ModelPrice {
    pub fn is_usable(&self) -> bool {
        self.samples > 0
    }

    /// Whether every component is a rate a provider could actually charge.
    ///
    /// Used both when a rate is learned and when one is restored from disk, so
    /// that a figure produced by an older build cannot come back on every
    /// restart. Decimal cannot hold a NaN or an infinity, so the range check is
    /// the whole story: a negative rate or one past the ceiling is an arithmetic
    /// accident, and a single one in the table sizes the hard gate -- and so
    /// refuses or admits every request that follows.
    pub fn is_plausible(&self) -> bool {
        let ceiling = max_plausible_per_token();
        [
            self.input_per_token,
            self.output_per_token,
            self.cache_read_per_token,
        ]
        .iter()
        .all(|p| *p >= Decimal::ZERO && *p <= ceiling)
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

        let input = sat_mul(Decimal::from(uncached), self.input_per_token);
        let cache_read = sat_mul(Decimal::from(cached), self.cache_read_per_token);
        let output = sat_mul(
            Decimal::from(usage.completion_tokens),
            self.output_per_token,
        );

        sat_add(input, sat_add(cache_read, output))
    }

    /// Cache-read price as a share of the input price.
    ///
    /// The learned rate is a single blended figure, so reducing a spend delta by
    /// a token count needs the cache read expressed relative to input.
    pub fn cache_read_ratio(&self) -> Decimal {
        if self.input_per_token > Decimal::ZERO {
            self.cache_read_per_token / self.input_per_token
        } else {
            Decimal::ZERO
        }
    }

    /// `base_cost` scaled by the account's credit multiplier.
    pub fn cost(&self, usage: &TokenUsage, multiplier: Decimal) -> Decimal {
        sat_mul(self.base_cost(usage), sanitize_multiplier(multiplier))
    }
}

/// Where a price came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    /// Set by the operator in the [prices] config table. THE EXPECTATION.
    Configured,
    /// Recovered from provider spend deltas. THE OBSERVED REALITY.
    Learned,
}

impl PriceSource {
    pub fn label(self) -> &'static str {
        match self {
            PriceSource::Configured => "configured",
            PriceSource::Learned => "learned",
        }
    }
}

/// One model's pricing under one provider: what was configured, and what was
/// observed.
///
/// These are kept apart on purpose. The configured price is the expectation and
/// is never overwritten by observation, so the two can be compared forever.
/// Divergence means the upstream price no longer matches what the operator
/// agreed to -- a real repricing, or a quiet one.
#[derive(Debug, Clone, Default)]
pub struct PriceEntry {
    pub configured: Option<ModelPrice>,
    pub learned: Option<ModelPrice>,
    /// Token volume that produced the learned figure.
    ///
    /// Kept because a configured prompt/completion split cannot be compared
    /// directly against a single blended learned rate: they are different
    /// shapes. Comparing the two COSTS over the same usage mix is the only fair
    /// comparison, and for that the mix has to be remembered.
    pub observed_prompt_tokens: u64,
    pub observed_completion_tokens: u64,
    pub observed_cached_tokens: u64,
}

impl PriceEntry {
    /// Which price to bill with. Observed reality wins over the expectation.
    pub fn effective(&self) -> Option<&ModelPrice> {
        self.learned.as_ref().or(self.configured.as_ref())
    }

    /// Cost of a usage sample under the account's credit multiplier.
    ///
    /// The multiplier converts a USD price into the account's billing unit
    /// (`spent_local += price(model, tokens) x credit_multiplier`), so it applies
    /// to the CONFIGURED price only. A
    /// learned price was recovered as "provider spend delta / weighted tokens";
    /// that spend is already in billing units, so multiplying it again squares
    /// the factor: a 4x multiplier -- the kind Command Code's models use --
    /// over-estimates 16x, which is merely early,
    /// while a multiplier below 1 under-estimates by the same factor, and
    /// under-estimating is the direction that lets spend slip through into
    /// metered.
    ///
    /// Callers that only want a share of spend, such as the router splitting an
    /// interval across tiers, pass Decimal::ONE and are unaffected either way.
    pub fn cost(&self, usage: &TokenUsage, multiplier: Decimal) -> Option<Decimal> {
        match (self.learned.as_ref(), self.configured.as_ref()) {
            (Some(learned), _) => Some(learned.base_cost(usage)),
            (None, Some(configured)) => Some(configured.cost(usage, multiplier)),
            (None, None) => None,
        }
    }

    pub fn source(&self) -> Option<PriceSource> {
        if self.learned.is_some() {
            Some(PriceSource::Learned)
        } else if self.configured.is_some() {
            Some(PriceSource::Configured)
        } else {
            None
        }
    }

    /// Share of tokens that were input, output and cache read, as percentages.
    ///
    /// The per-million figures are only interpretable next to this: the same
    /// prices produce very different $/M on different mixes.
    pub fn mix_percentages(&self) -> Option<(f64, f64, f64)> {
        let total = self
            .observed_prompt_tokens
            .saturating_add(self.observed_completion_tokens);
        if total == 0 {
            return None;
        }
        let t = total as f64;
        // Cache read is a subset of the prompt tokens in the OpenAI dialect, so
        // it is shown as its own share of the total for readability and the
        // input share is what remains.
        let cached = self.observed_cached_tokens.min(self.observed_prompt_tokens);
        let plain_input = self.observed_prompt_tokens - cached;
        Some((
            plain_input as f64 / t * 100.0,
            self.observed_completion_tokens as f64 / t * 100.0,
            cached as f64 / t * 100.0,
        ))
    }

    /// The usage this entry has accumulated, for drift comparison.
    pub fn observed_usage(&self) -> TokenUsage {
        TokenUsage {
            prompt_tokens: self.observed_prompt_tokens,
            completion_tokens: self.observed_completion_tokens,
            cached_tokens: self.observed_cached_tokens,
            reasoning_tokens: 0,
            measured: true,
        }
    }
}

/// How far the observed price has drifted from the configured expectation.
#[derive(Debug, Clone, Copy)]
pub struct PriceDrift {
    /// What the configured price says the accumulated usage should have cost.
    pub expected: Decimal,
    /// What the learned rate says it actually cost.
    pub observed: Decimal,
    /// The same two figures per million tokens, over the SAME observed mix, so
    /// they can be read side by side: "I expected $0.22/M, I am paying $0.90/M".
    ///
    /// Never compare a per-million figure across different mixes. It moves with
    /// the mix even when the prices have not changed at all: identical prices
    /// produced $0.2166/M on an input-heavy mix and $0.9000/M on an
    /// output-heavy one. That is why the ratio, which is zero whenever prices
    /// match regardless of mix, remains the alarm and these are the explanation.
    pub expected_per_million: Decimal,
    pub actual_per_million: Decimal,
    /// Token volume behind the two figures.
    pub total_tokens: u64,
    /// observed / expected - 1. Positive means reality is MORE EXPENSIVE than
    /// configured, which is the direction that threatens the hard gate.
    pub ratio: f64,
    pub samples: u64,
    /// Whether the divergence is large enough to act on.
    pub significant: bool,
}

/// Relative divergence at or above this is reported. Small differences are
/// normal: the learned rate is a blended approximation, and the configured
/// split is rounded from a pricing page.
pub const PRICE_DRIFT_THRESHOLD: f64 = 0.20;

/// A price as it survives a restart.
#[derive(Debug, Clone, PartialEq)]
pub struct PersistedPrice {
    pub provider: String,
    pub model: String,
    /// 0 is the default tier; higher values are configured windows.
    pub tier: u32,
    pub price: ModelPrice,
    pub observed_prompt_tokens: u64,
    pub observed_completion_tokens: u64,
    pub observed_cached_tokens: u64,
}

/// Effective prices per (provider, model).
///
/// Keyed by provider as well as model because the same model can be billed
/// differently by different providers: one sells tokens at list price, another
/// meters them against a plan's credit allowance.
#[derive(Debug, Clone, Default)]
pub struct PriceTable {
    entries: BTreeMap<(String, String, u32), PriceEntry>,
}

impl PriceTable {
    pub fn new() -> Self {
        PriceTable::default()
    }

    /// Price identity is (provider, model, tier). Tier 0 is the default price,
    /// charged outside every configured window; higher tiers are the windows.
    fn key(provider: &str, model: &str, tier: u32) -> (String, String, u32) {
        (provider.to_string(), model.to_string(), tier)
    }

    /// Billing price for a model: the learned rate when there is one, otherwise
    /// the configured price.
    pub fn effective(&self, provider: &str, model: &str, tier: u32) -> Option<&ModelPrice> {
        self.entries
            .get(&Self::key(provider, model, tier))
            .and_then(PriceEntry::effective)
    }

    pub fn entry(&self, provider: &str, model: &str, tier: u32) -> Option<&PriceEntry> {
        self.entries.get(&Self::key(provider, model, tier))
    }

    /// Every entry, for the prices command.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str, u32, &PriceEntry)> {
        self.entries
            .iter()
            .map(|((p, m, t), e)| (p.as_str(), m.as_str(), *t, e))
    }

    pub fn is_known(&self, provider: &str, model: &str, tier: u32) -> bool {
        self.entries
            .get(&Self::key(provider, model, tier))
            .is_some_and(|e| e.effective().is_some())
    }

    pub fn known_models(&self) -> usize {
        self.entries
            .values()
            .filter(|e| e.effective().is_some())
            .count()
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
    ///
    /// The multiplier is applied by `PriceEntry::cost`: to a configured price,
    /// which is quoted in USD, but not to a learned one, which is already in the
    /// account's billing unit.
    pub fn estimate(
        &self,
        provider: &str,
        model: &str,
        tier: u32,
        usage: &TokenUsage,
        multiplier: Decimal,
    ) -> Option<Decimal> {
        self.entries
            .get(&Self::key(provider, model, tier))
            .and_then(|entry| entry.cost(usage, multiplier))
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
    /// single BLENDED rate in the account's billing unit per weighted token,
    /// applied to both prompt and completion tokens. Cached prompt tokens are
    /// weighted by the previous observation's discount (0.1 by default), which
    /// is correct as long as the cached/uncached mix stays stable between
    /// probes. Repeated observations are averaged by the token VOLUME behind
    /// them, so a single heavy interval dominates a string of tiny ones.
    /// Weighting by sample COUNT did the opposite: ten 0.25 USD/M intervals over
    /// 1,000 tokens each plus one 900 USD/M interval over a million tokens
    /// learned about 82 USD/M, when 99.99% of the spend came from the
    /// million-token interval. A config-seeded prior (samples == 0) is replaced
    /// outright by the first real observation.
    ///
    /// Callers must only pass real billing deltas: an interval with no traffic
    /// (weighted tokens == 0) and a negative cost are ignored. A zero cost over
    /// a non-zero token delta IS learned, at rate 0 -- that is how free models
    /// such as "laguna-s-2.1-free" end up costing nothing and never trigger a
    /// rotation.
    pub fn observe(
        &mut self,
        provider: &str,
        model: &str,
        tier: u32,
        cost: Decimal,
        usage: &TokenUsage,
    ) {
        if cost < Decimal::ZERO {
            return;
        }
        let cached = usage.cached_tokens.min(usage.prompt_tokens);
        let uncached = usage.prompt_tokens - cached;

        let entry = self
            .entries
            .entry(Self::key(provider, model, tier))
            .or_default();
        // A configured price carries samples == 0 and is only a starting point,
        // so it is replaced outright by the first real observation rather than
        // averaged with it. Averaging an expectation into reality would hide
        // exactly the drift this comparison exists to reveal.
        let previous = match entry.learned {
            Some(p) if p.samples > 0 => Some(p),
            _ => None,
        };
        // Cache reads are priced at a share of input. Carry that share over from
        // whatever we already know: the learned figure, else the configured one,
        // else the provider-standard tenth.
        let ratio = previous
            .map(|p| p.cache_read_ratio())
            .or_else(|| entry.configured.map(|p| p.cache_read_ratio()))
            .unwrap_or_else(default_cache_read_ratio);

        let weighted = sat_add(
            Decimal::from(uncached),
            sat_add(
                sat_mul(Decimal::from(cached), ratio),
                Decimal::from(usage.completion_tokens),
            ),
        );
        if weighted <= Decimal::ZERO {
            return;
        }

        let observed = cost / weighted;
        // A rate past MAX_PLAUSIBLE_PER_MILLION is nonsense and would wedge the
        // hard gate -- refusing every request for the life of the ledger.
        // Ignore it rather than let one bad interval poison the table.
        // A zero rate is deliberately NOT refused: a model that really is free
        // must fall out at rate 0 and never trigger a rotation. Only the
        // ceiling is.
        if observed > max_plausible_per_token() {
            return;
        }

        let prior_samples = previous.map(|p| p.samples).unwrap_or(0);
        // The weight of a rate is the token volume it was derived from, not the
        // number of times it was observed. `observed_*` accumulates exactly the
        // usage of every accepted observation, so it is that volume. A learned
        // rate with no recorded volume -- a price installed by `set`, say -- has
        // nothing to weigh, so the first observation replaces it rather than
        // being averaged with it.
        let previous_weight = weighted_tokens(&entry.observed_usage(), ratio);
        let (input_per_token, output_per_token) = match previous {
            Some(p) if previous_weight > Decimal::ZERO => {
                // Volume-weighted mean: (rate_a x volume_a + rate_b x volume_b)
                // / (volume_a + volume_b). Each term is in spend units, so the
                // result stays in billing units per token.
                let weight = sat_add(previous_weight, weighted);
                (
                    sat_add(
                        sat_mul(p.input_per_token, previous_weight),
                        sat_mul(observed, weighted),
                    ) / weight,
                    sat_add(
                        sat_mul(p.output_per_token, previous_weight),
                        sat_mul(observed, weighted),
                    ) / weight,
                )
            }
            // One equation cannot separate an input rate from an output rate, so
            // the learned figure is a single blended rate applied to both. The
            // configured price keeps its own split, which is why the drift
            // comparison is made on cost rather than on the two rates.
            _ => (observed, observed),
        };
        // Keep the cache-read share we already had, expressed against the new
        // blended input rate so the relationship survives learning.
        let cache_read_per_token = previous
            .map(|p| p.cache_read_ratio())
            .or_else(|| entry.configured.map(|p| p.cache_read_ratio()))
            .map(|r| sat_mul(input_per_token, r))
            .unwrap_or_else(|| sat_mul(input_per_token, default_cache_read_ratio()));

        entry.learned = Some(ModelPrice {
            input_per_token,
            output_per_token,
            cache_read_per_token,
            samples: prior_samples.saturating_add(1),
        });
        entry.observed_prompt_tokens = entry
            .observed_prompt_tokens
            .saturating_add(usage.prompt_tokens);
        entry.observed_completion_tokens = entry
            .observed_completion_tokens
            .saturating_add(usage.completion_tokens);
        entry.observed_cached_tokens = entry.observed_cached_tokens.saturating_add(cached);
    }

    /// Install prices from the [prices] config table (USD per million tokens).
    ///
    /// These are the operator's EXPECTATION and double as the starting point for
    /// learning: the learned value replaces them on the first observation, while
    /// the configured value is kept unchanged as the reference for drift.
    ///
    /// Returns how many were installed; entries with a non-finite or negative
    /// price are skipped rather than clamped, because a bad price silently
    /// becoming zero is the one error that spends money.
    pub fn seed_from_config(
        &mut self,
        prices: &BTreeMap<String, BTreeMap<String, ModelPriceConfig>>,
    ) -> usize {
        let mut installed = 0;
        for (provider, models) in prices {
            for (model, p) in models {
                // Tier 0 is the default price; each schedule becomes its own
                // tier with its own configured price and its own learned rate.
                // Keying by tier is what lets the hard gate price a request by
                // the time it started, and what lets drift be measured per
                // window instead of being smeared across both.
                for (tier, t) in p.tiers().into_iter().enumerate() {
                    let (Some(input), Some(output)) = (
                        per_token_from_million(t.input),
                        per_token_from_million(t.output),
                    ) else {
                        continue;
                    };
                    // A cache-read price is optional. When it is absent, or when
                    // it is not usable, fall back to that tier's input price over
                    // the provider-standard share rather than to zero: a
                    // free-looking cache read would under-estimate cost, which is
                    // the spending direction.
                    let cache_read = per_token_from_million(t.cache_read)
                        .filter(|c| *c >= Decimal::ZERO)
                        .unwrap_or_else(|| sat_mul(input, default_cache_read_ratio()));
                    let entry = self
                        .entries
                        .entry(Self::key(provider, model, tier as u32))
                        .or_default();
                    entry.configured = Some(ModelPrice {
                        input_per_token: input,
                        output_per_token: output,
                        cache_read_per_token: cache_read,
                        samples: 0,
                    });
                    installed += 1;
                }
            }
        }
        installed
    }

    /// Restore prices learned in an earlier run.
    ///
    /// Without this, every restart falls back to the conservative default until
    /// the next probe, and that default was measured against live providers to
    /// be off by an order of magnitude in both directions.
    ///
    /// Rows are re-checked against the plausibility ceiling on the way in. A
    /// ceiling is a property of a price, not of the path that produced it, and a
    /// rate written by an older build would otherwise be restored at every
    /// startup for the life of the ledger -- with no way back except deleting
    /// the row by hand. Skipping it here also repairs a ledger that already
    /// carries one.
    pub fn load_learned(&mut self, rows: Vec<PersistedPrice>) -> usize {
        let mut loaded = 0;
        for row in rows {
            if row.model.is_empty() || row.provider.is_empty() {
                continue;
            }
            if !row.price.is_plausible() {
                continue;
            }
            let entry = self
                .entries
                .entry(Self::key(&row.provider, &row.model, row.tier))
                .or_default();
            entry.learned = Some(row.price);
            entry.observed_prompt_tokens = row.observed_prompt_tokens;
            entry.observed_completion_tokens = row.observed_completion_tokens;
            entry.observed_cached_tokens = row.observed_cached_tokens;
            loaded += 1;
        }
        loaded
    }

    /// Everything worth persisting: learned rates and the usage behind them.
    /// Configured prices come from the config file and are not stored here.
    pub fn learned_rows(&self) -> Vec<PersistedPrice> {
        self.entries
            .iter()
            .filter_map(|((provider, model, tier), e)| {
                let price = e.learned?;
                Some(PersistedPrice {
                    provider: provider.clone(),
                    model: model.clone(),
                    tier: *tier,
                    price,
                    observed_prompt_tokens: e.observed_prompt_tokens,
                    observed_completion_tokens: e.observed_completion_tokens,
                    observed_cached_tokens: e.observed_cached_tokens,
                })
            })
            .collect()
    }

    pub fn set(&mut self, provider: &str, model: &str, tier: u32, price: ModelPrice) {
        self.entries
            .entry(Self::key(provider, model, tier))
            .or_default()
            .learned = Some(price);
    }

    pub fn get(&self, provider: &str, model: &str, tier: u32) -> Option<&ModelPrice> {
        self.effective(provider, model, tier)
    }

    /// What the observed spend works out to per million tokens, at the mix that
    /// produced it.
    ///
    /// Available without a configured price, because it is a useful figure on
    /// its own. Returns the figure and the token volume behind it.
    pub fn actual_per_million(
        &self,
        provider: &str,
        model: &str,
        tier: u32,
    ) -> Option<(Decimal, u64)> {
        let entry = self.entries.get(&Self::key(provider, model, tier))?;
        let learned = entry.learned.filter(|p| p.samples > 0)?;
        let usage = entry.observed_usage();
        let total = usage.total_tokens();
        if total == 0 {
            return None;
        }
        let weighted = weighted_tokens(&usage, learned.cache_read_ratio());
        let cost = sat_mul(learned.input_per_token, weighted);
        Some((
            sat_mul(cost, Decimal::from(1_000_000u64)) / Decimal::from(total),
            total,
        ))
    }

    /// How far reality has diverged from the configured expectation.
    ///
    /// None when either side is missing: with no configured price there is
    /// nothing to compare against, and with no observation there is no reality
    /// yet.
    pub fn drift(&self, provider: &str, model: &str, tier: u32) -> Option<PriceDrift> {
        let entry = self.entries.get(&Self::key(provider, model, tier))?;
        let configured = entry.configured?;
        let learned = entry.learned.filter(|p| p.samples > 0)?;
        let usage = entry.observed_usage();
        if usage.total_tokens() == 0 {
            return None;
        }

        let expected = configured.cost(&usage, Decimal::ONE);
        if expected <= Decimal::ZERO {
            return None;
        }
        let weighted = weighted_tokens(&usage, learned.cache_read_ratio());
        let observed = sat_mul(learned.input_per_token, weighted);

        let ratio = {
            use rust_decimal::prelude::ToPrimitive;
            let e = expected.to_f64().unwrap_or(0.0);
            let o = observed.to_f64().unwrap_or(0.0);
            if e > 0.0 { (o / e) - 1.0 } else { 0.0 }
        };

        let total_tokens = usage.total_tokens();
        let per_million = |cost: Decimal| -> Decimal {
            sat_mul(cost, Decimal::from(1_000_000u64)) / Decimal::from(total_tokens)
        };

        Some(PriceDrift {
            expected,
            observed,
            expected_per_million: per_million(expected),
            actual_per_million: per_million(observed),
            total_tokens,
            ratio,
            samples: learned.samples,
            significant: ratio.abs() >= PRICE_DRIFT_THRESHOLD,
        })
    }

    /// Every entry whose observed price has drifted materially from the
    /// configured one, worst first. This is the list an operator actually wants:
    /// each row is a model that no longer costs what they were told.
    pub fn drifted(&self) -> Vec<(String, String, u32, PriceDrift)> {
        let mut out: Vec<(String, String, u32, PriceDrift)> = self
            .entries
            .keys()
            .filter_map(|(provider, model, tier)| {
                let d = self.drift(provider, model, *tier)?;
                if d.significant {
                    Some((provider.clone(), model.clone(), *tier, d))
                } else {
                    None
                }
            })
            .collect();
        out.sort_by(|a, b| {
            b.3.ratio
                .abs()
                .partial_cmp(&a.3.ratio.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }
}

/// Share of the input price a cache read costs when nothing better is known.
fn default_cache_read_ratio() -> Decimal {
    Decimal::try_from(crate::config::default_cache_read_ratio())
        .unwrap_or_else(|_| Decimal::new(1, 1))
}

/// Tokens weighted by how much they actually cost.
///
/// Cached prompt tokens are billed at a fraction of the uncached rate, so a raw
/// token count cannot be divided into a spend delta. This is the denominator
/// that can: uncached prompt tokens at full weight, cached at the discount, and
/// completion tokens at full weight.
fn weighted_tokens(usage: &TokenUsage, cached_discount: Decimal) -> Decimal {
    let cached = usage.cached_tokens.min(usage.prompt_tokens);
    let uncached = usage.prompt_tokens - cached;
    sat_add(
        Decimal::from(uncached),
        sat_add(
            sat_mul(Decimal::from(cached), cached_discount),
            Decimal::from(usage.completion_tokens),
        ),
    )
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
                sat_mul(
                    sat_mul(Decimal::from(cached), prompt),
                    default_cached_discount(),
                ),
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

/// A generation window shorter than this cannot be measured: the provider
/// buffered its output and delivered it in one burst.
pub const MIN_TPS_WINDOW: std::time::Duration = std::time::Duration::from_millis(100);

/// No LLM API streams faster than this. A higher figure is an artifact of
/// buffered delivery, not model speed.
pub const MAX_PLAUSIBLE_TPS: f64 = 1000.0;

/// Tokens per second over the generation window only. The time to first token
/// is deliberately excluded from the denominator.
///
/// Pass the instant the first content chunk arrived and the instant of the last
/// content chunk. Returns None rather than a misleading figure when the result
/// is not attributable to generation speed:
///   - no completion tokens were produced;
///   - the window is empty, or time ran backwards;
///   - the window is shorter than MIN_TPS_WINDOW, which is what a buffered
///     upstream looks like: several content frames arriving in one read;
///   - the result exceeds MAX_PLAUSIBLE_TPS, which no real provider reaches.
///
/// Measured against live providers, buffered delivery produced figures such as
/// 14818 and 77170 tokens per second. Those are more damaging than a blank,
/// because they look like data and would be used to compare providers.
pub fn compute_tps(
    completion_tokens: u64,
    first_content: Instant,
    last_content: Instant,
) -> Option<f64> {
    if completion_tokens == 0 {
        return None;
    }
    let window = last_content.checked_duration_since(first_content)?;
    if window < MIN_TPS_WINDOW {
        return None;
    }
    let tps = completion_tokens as f64 / window.as_secs_f64();
    if !tps.is_finite() || tps > MAX_PLAUSIBLE_TPS {
        return None;
    }
    Some(tps)
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

/// Ceiling on a learnable rate, in USD per million tokens.
///
/// Above this a figure is not a price, it is a mis-attribution: a fixed
/// per-request charge, or another client's traffic on a shared account, divided
/// by an interval that happened to carry almost no tokens. The bound used to be
/// one dollar per TOKEN -- a million dollars per million -- which nothing could
/// reach, so it never guarded anything. The dearest production models bill
/// around 150 USD/M, so 1000 leaves an order of magnitude of room and still
/// catches a rate like the 1559 USD/M that a 47-token interval produced once.
pub const MAX_PLAUSIBLE_PER_MILLION: u64 = 1000;

fn max_plausible_per_token() -> Decimal {
    per_token(MAX_PLAUSIBLE_PER_MILLION)
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

    /// Provider used by every price test. Prices are keyed by (provider, model).
    const P: &str = "prov";

    /// input and output are USD per million tokens; cache_read_scale is the
    /// share of input that a cache read costs (2 means a half, 1 a tenth).
    fn price(input: i64, output: i64, cache_read_scale: u32) -> ModelPrice {
        let input_per_token = Decimal::new(input, 6);
        ModelPrice {
            input_per_token,
            output_per_token: Decimal::new(output, 6),
            cache_read_per_token: input_per_token / Decimal::from(10u32.pow(cache_read_scale)),
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

    /// A table whose only price is the configured one, so a test exercises the
    /// USD -> billing-unit conversion rather than a learned rate.
    fn seeded(input: f64, output: f64) -> PriceTable {
        use crate::config::ModelPriceConfig;
        let mut cfg: BTreeMap<String, BTreeMap<String, ModelPriceConfig>> = BTreeMap::new();
        cfg.entry(P.to_string()).or_default().insert(
            "m".to_string(),
            ModelPriceConfig {
                input,
                output,
                cache_read: None,
                schedules: Vec::new(),
            },
        );
        let mut table = PriceTable::new();
        table.seed_from_config(&cfg);
        table
    }

    #[test]
    fn cached_prompt_token_costs_less_than_an_uncached_one() {
        let mut table = PriceTable::new();
        table.set(P, "m", 0, price(3, 15, 1)); // $3 / $15 per million

        let uncached = table
            .estimate(P, "m", 0, &usage(1_000, 0, 0), Decimal::ONE)
            .unwrap();
        let cached = table
            .estimate(P, "m", 0, &usage(1_000, 0, 1_000), Decimal::ONE)
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
        table.set(P, "m", 0, price(3, 15, 1));
        let cost = table
            .estimate(P, "m", 0, &usage(0, 1_000, 0), Decimal::ONE)
            .unwrap();
        assert_eq!(cost, Decimal::new(15, 3)); // 1000 * 0.000015
    }

    /// A configured price is quoted in USD, so it is the one the multiplier
    /// converts into the account's billing unit.
    #[test]
    fn a_configured_price_is_converted_to_billing_units_by_the_multiplier() {
        let table = seeded(3.0, 15.0);
        let u = usage(1_000, 1_000, 0);

        let one = table.estimate(P, "m", 0, &u, Decimal::ONE).unwrap();
        assert_eq!(one, Decimal::new(18, 3)); // 1000 x 3/M + 1000 x 15/M
        let four = table.estimate(P, "m", 0, &u, Decimal::from(4)).unwrap();
        assert_eq!(four, one * Decimal::from(4));

        // A zero or negative multiplier must not make a request look free: the
        // hard gate would then admit everything.
        assert_eq!(table.estimate(P, "m", 0, &u, Decimal::ZERO).unwrap(), one);
        assert_eq!(
            table
                .estimate(P, "m", 0, &u, Decimal::NEGATIVE_ONE)
                .unwrap(),
            one
        );
    }

    /// Billing charges price(model, tokens) x credit_multiplier, and observe
    /// recovers that price from provider-reported spend -- spend that has
    /// ALREADY been multiplied. Applying the multiplier to the learned price
    /// again squares it: a factor above 1 only makes the gate early, but a factor
    /// below 1 under-estimates by the same amount and lets spend slip through
    /// into metered.
    #[test]
    fn the_credit_multiplier_scales_a_configured_price_but_not_a_learned_one() {
        let mut table = seeded(3.0, 15.0);
        let u = usage(1_000, 1_000, 0);

        let configured = table.estimate(P, "m", 0, &u, Decimal::ONE).unwrap();
        assert_eq!(
            table.estimate(P, "m", 0, &u, Decimal::from(2)).unwrap(),
            configured * Decimal::from(2),
            "a configured USD price is converted by the multiplier"
        );

        // 0.10 of provider-reported spend over 2,000 tokens learns 0.00005 per
        // token, i.e. 50 USD/M in the account's own unit.
        table.observe(P, "m", 0, Decimal::new(1, 1), &usage(2_000, 0, 0));
        let learned = *table.get(P, "m", 0).unwrap();
        assert_eq!(learned.input_per_token, Decimal::new(5, 5));

        let once = table.estimate(P, "m", 0, &u, Decimal::ONE).unwrap();
        assert_eq!(
            once,
            learned.base_cost(&u),
            "learned rate x tokens, nothing else"
        );
        assert_eq!(once, Decimal::new(1, 1), "2000 tokens x 0.00005 = 0.10");
        assert_eq!(
            table.estimate(P, "m", 0, &u, Decimal::from(2)).unwrap(),
            once,
            "the multiplier is already inside the learned rate and must not be applied twice"
        );
    }

    #[test]
    fn unknown_model_estimates_none_not_zero() {
        let table = PriceTable::new();
        let u = usage(5_000, 500, 0);
        assert_eq!(
            table.estimate(P, "nobody/knows-this", 0, &u, Decimal::ONE),
            None
        );
        assert!(!table.is_known(P, "nobody/knows-this", 0));
        // ... and the explicit fallback is strictly positive, never free.
        assert!(conservative_estimate(&u, Decimal::ONE) > Decimal::ZERO);
    }

    #[test]
    fn conservative_estimate_is_above_a_plausible_real_price() {
        let mut table = PriceTable::new();
        table.set(P, "m", 0, price(3, 15, 1));
        let u = usage(10_000, 1_000, 0);
        let fallback = conservative_estimate(&u, Decimal::ONE);
        let real = table.estimate(P, "m", 0, &u, Decimal::ONE).unwrap();
        assert!(
            fallback > real,
            "the unknown-model fallback ({fallback}) must over-estimate the known price ({real})"
        );
    }

    #[test]
    fn observe_learns_the_effective_rate_from_a_delta() {
        let mut table = PriceTable::new();
        // Ten cents of delta spend over 1000 prompt + 1000 completion tokens.
        table.observe(P, "m", 0, Decimal::new(10, 2), &usage(1_000, 1_000, 0));

        let learned = table.get(P, "m", 0).unwrap();
        assert_eq!(learned.samples, 1);
        assert_eq!(learned.input_per_token, Decimal::new(5, 5)); // 0.10 / 2000
        assert_eq!(learned.output_per_token, Decimal::new(5, 5));
        // Replaying the same delta through the learned rate returns the same cost.
        let cost = table
            .estimate(P, "m", 0, &usage(1_000, 1_000, 0), Decimal::ONE)
            .unwrap();
        assert_eq!(cost, Decimal::new(10, 2));
    }

    #[test]
    fn observe_weights_cached_tokens_by_the_discount() {
        let mut table = PriceTable::new();
        // All 1000 prompt tokens were cache reads: weight = 1000 * 0.1 = 100.
        table.observe(P, "m", 0, Decimal::new(1, 2), &usage(1_000, 0, 1_000));
        assert_eq!(
            table.get(P, "m", 0).unwrap().input_per_token,
            Decimal::new(1, 4) // 0.01 / 100
        );
    }

    #[test]
    fn observe_averages_repeated_observations() {
        let mut table = PriceTable::new();
        // 0.002 and 0.006 over 1000-token intervals: 2 and 6 USD per million.
        // These used to be 1 and 3, which are 1000 and 3000 USD per million --
        // the averaging was being proved on rates no provider charges.
        table.observe(P, "m", 0, Decimal::new(2, 3), &usage(1_000, 0, 0));
        table.observe(P, "m", 0, Decimal::new(6, 3), &usage(1_000, 0, 0));
        let learned = table.get(P, "m", 0).unwrap();
        assert_eq!(learned.samples, 2);
        assert_eq!(learned.input_per_token, Decimal::new(4, 6)); // mean 4 USD/M
        assert_eq!(
            table
                .estimate(P, "m", 0, &usage(1_000, 0, 0), Decimal::ONE)
                .unwrap(),
            Decimal::new(4, 3)
        );
    }

    /// Averaging a price by sample COUNT lets ten tiny intervals outvote the one
    /// interval that carried the spend. Real cost is dominated by volume, so the
    /// learned rate has to be weighted by the token volume behind each
    /// observation: ten 0.25 USD/M intervals over 1,000 tokens each ($0.00025
    /// apiece) plus one 900 USD/M interval over a million tokens ($900) is a
    /// rate near 891 USD/M, not the 82 USD/M a per-observation mean reports.
    #[test]
    fn a_heavy_interval_outweighs_many_small_ones_when_learning_a_price() {
        let mut table = PriceTable::new();
        for _ in 0..10 {
            // 0.00025 over 1,000 tokens = 0.25 USD/M.
            table.observe(P, "m", 0, Decimal::new(25, 5), &usage(1_000, 0, 0));
        }
        // 900 USD over 1,000,000 tokens = 900 USD/M, and 99.99% of the spend.
        table.observe(P, "m", 0, Decimal::from(900), &usage(1_000_000, 0, 0));

        let learned = table.get(P, "m", 0).unwrap();
        assert_eq!(learned.samples, 11, "samples still counts observations");
        // (0.25/M x 10k tokens + 900/M x 1M tokens) / 1.01M tokens = 891.1 USD/M.
        assert!(
            learned.input_per_token > Decimal::new(800, 6)
                && learned.input_per_token < Decimal::new(950, 6),
            "the heavy interval must dominate, got {} USD/M (the per-count mean is 82 USD/M)",
            learned.input_per_token * Decimal::from(1_000_000u64)
        );
    }

    /// A rate derived from an interval with almost no tokens is not a price.
    ///
    /// Measured live: one probe interval on a Command Code account carried 47
    /// tokens and reported 0.073277008 credits of spend, which reads as 1559 USD
    /// per million. The old ceiling was one dollar per TOKEN -- a million per
    /// million -- so no observation could ever exceed it. The table took the
    /// rate, and from then on every request larger than roughly 5,400 estimated
    /// tokens was refused by the hard gate, for the life of the ledger.
    #[test]
    fn a_rate_that_would_wedge_the_hard_gate_is_refused() {
        let mut table = PriceTable::new();
        table.observe(P, "m", 0, Decimal::new(73_277_008, 9), &usage(31, 16, 0));
        assert!(
            table.get(P, "m", 0).is_none(),
            "an implausible rate must not be learned"
        );
        assert_eq!(table.known_models(), 0);
        // ... and the gate falls back to the deliberately conservative default.
        let est = crate::meter::conservative_estimate(&usage(31, 16, 0), Decimal::ONE);
        assert!(est > Decimal::ZERO);
        assert!(
            table
                .estimate(P, "m", 0, &usage(31, 16, 0), Decimal::ONE)
                .is_none(),
            "with nothing learned the caller must use the conservative estimate"
        );
    }

    /// The same ceiling has to apply on the way in from disk.
    ///
    /// Otherwise a rate written by an older build is restored at every startup
    /// for the life of the ledger, and the only way back is deleting the row by
    /// hand. Checking here also means a ledger that already carries one heals
    /// itself on the next restart.
    #[test]
    fn a_poisoned_price_on_disk_is_skipped_rather_than_restored() {
        let poisoned: Decimal = "0.0015590852765957446808510638".parse().unwrap();
        let rows = vec![PersistedPrice {
            provider: P.to_string(),
            model: "m".to_string(),
            tier: 0,
            price: ModelPrice {
                input_per_token: poisoned,
                output_per_token: poisoned,
                cache_read_per_token: poisoned,
                samples: 1,
            },
            observed_prompt_tokens: 31,
            observed_completion_tokens: 16,
            observed_cached_tokens: 0,
        }];

        let mut table = PriceTable::new();
        assert_eq!(table.load_learned(rows), 0);
        assert!(table.get(P, "m", 0).is_none());
        assert_eq!(table.known_models(), 0);
    }

    #[test]
    fn observe_learns_zero_for_a_free_model() {
        // Free models must fall out as a zero rate so they never look like they
        // are burning quota.
        let mut table = PriceTable::new();
        table.observe(
            P,
            "laguna-s-2.1-free",
            0,
            Decimal::ZERO,
            &usage(500, 100, 0),
        );
        let learned = table.get(P, "laguna-s-2.1-free", 0).unwrap();
        assert_eq!(learned.samples, 1);
        assert_eq!(learned.input_per_token, Decimal::ZERO);
        assert_eq!(
            table
                .estimate(P, "laguna-s-2.1-free", 0, &usage(500, 100, 0), Decimal::ONE)
                .unwrap(),
            Decimal::ZERO
        );
    }

    #[test]
    fn observe_ignores_an_interval_with_no_traffic_or_negative_cost() {
        let mut table = PriceTable::new();
        table.observe(P, "m", 0, Decimal::from(1), &usage(0, 0, 0));
        table.observe(P, "m", 0, Decimal::NEGATIVE_ONE, &usage(10, 10, 0));
        assert_eq!(table.known_models(), 0);
    }

    #[test]
    fn observe_rejects_an_implausible_rate() {
        let mut table = PriceTable::new();
        table.observe(P, "m", 0, Decimal::from(1_000_000), &usage(1, 0, 0));
        assert_eq!(
            table.known_models(),
            0,
            "a 1M USD per token rate must not be learned"
        );
    }

    #[test]
    fn config_seeded_priors_are_used_and_replaced_by_the_first_observation() {
        let mut prices: BTreeMap<String, BTreeMap<String, ModelPriceConfig>> = BTreeMap::new();
        prices.entry(P.to_string()).or_default().insert(
            "m".to_string(),
            ModelPriceConfig {
                input: 3.0,
                output: 15.0,
                cache_read: None,
                schedules: Vec::new(),
            },
        );

        let mut table = PriceTable::new();
        assert_eq!(table.seed_from_config(&prices), 1);
        assert!(table.is_known(P, "m", 0));
        assert!(
            !table.get(P, "m", 0).unwrap().is_usable(),
            "a configured prior has samples = 0"
        );
        assert_eq!(
            table.estimate(P, "m", 0, &usage(1_000_000, 0, 0), Decimal::ONE),
            Some(Decimal::from(3))
        );

        // The first real observation replaces the prior outright.
        table.observe(P, "m", 0, Decimal::new(5, 2), &usage(1_000, 0, 0));
        assert_eq!(table.get(P, "m", 0).unwrap().samples, 1);
        assert_eq!(
            table.get(P, "m", 0).unwrap().input_per_token,
            Decimal::new(5, 5)
        );
    }

    #[test]
    fn config_seeded_priors_skip_broken_prices() {
        let mut prices: BTreeMap<String, BTreeMap<String, ModelPriceConfig>> = BTreeMap::new();
        let models = prices.entry(P.to_string()).or_default();
        models.insert(
            "bad".to_string(),
            ModelPriceConfig {
                input: f64::NAN,
                output: 1.0,
                cache_read: None,
                schedules: Vec::new(),
            },
        );
        models.insert(
            "negative".to_string(),
            ModelPriceConfig {
                input: -1.0,
                output: 1.0,
                cache_read: None,
                schedules: Vec::new(),
            },
        );
        let mut table = PriceTable::new();
        assert_eq!(table.seed_from_config(&prices), 0);
        assert_eq!(
            table.estimate(P, "bad", 0, &usage(1, 1, 0), Decimal::ONE),
            None
        );
    }

    #[test]
    fn learned_prices_survive_a_round_trip() {
        let mut table = PriceTable::new();
        table.observe(P, "m", 0, Decimal::new(5, 2), &usage(1_000, 500, 100));
        assert_eq!(table.learned_rows().len(), 1);

        // A fresh table restores what the first one learned.
        let mut restored = PriceTable::new();
        assert_eq!(restored.load_learned(table.learned_rows()), 1);
        assert_eq!(
            restored.get(P, "m", 0).unwrap().input_per_token,
            table.get(P, "m", 0).unwrap().input_per_token
        );
        assert_eq!(restored.get(P, "m", 0).unwrap().samples, 1);
        // The usage behind the rate comes back too, so drift stays comparable.
        let entry = restored.entry(P, "m", 0).unwrap();
        assert_eq!(entry.observed_prompt_tokens, 1_000);
        assert_eq!(entry.observed_completion_tokens, 500);
        assert_eq!(entry.observed_cached_tokens, 100);
    }

    #[test]
    fn a_model_with_no_configured_price_has_nothing_to_drift_from() {
        let mut table = PriceTable::new();
        table.observe(P, "m", 0, Decimal::from(1), &usage(1_000, 0, 0));
        assert!(table.drift(P, "m", 0).is_none());
        assert!(table.drifted().is_empty());
    }

    #[test]
    fn drift_is_reported_only_when_reality_moves_away_from_the_configured_price() {
        let mut prices: BTreeMap<String, BTreeMap<String, ModelPriceConfig>> = BTreeMap::new();
        prices.entry(P.to_string()).or_default().insert(
            "m".to_string(),
            ModelPriceConfig {
                input: 3.0,
                output: 15.0,
                cache_read: None,
                schedules: Vec::new(),
            },
        );

        // Reality matches the expectation: 1M prompt tokens at $3/M.
        let mut table = PriceTable::new();
        table.seed_from_config(&prices);
        table.observe(P, "m", 0, Decimal::from(3), &usage(1_000_000, 0, 0));
        let d = table.drift(P, "m", 0).expect("drift needs both sides");
        assert!(
            d.ratio.abs() < 0.01,
            "a matching price must not look like drift, got {:+.1}%",
            d.ratio * 100.0
        );
        assert!(!d.significant);
        assert!(table.drifted().is_empty());

        // Now double the observed spend for the same tokens: +100%.
        let mut table = PriceTable::new();
        table.seed_from_config(&prices);
        table.observe(P, "m", 0, Decimal::from(6), &usage(1_000_000, 0, 0));
        let d = table.drift(P, "m", 0).unwrap();
        assert!((d.ratio - 1.0).abs() < 0.01, "got {:+.1}%", d.ratio * 100.0);
        assert!(d.significant, "a doubling must be reported");
        assert_eq!(d.expected, Decimal::from(3));
        assert_eq!(d.observed, Decimal::from(6));

        let all = table.drifted();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, P);
        assert_eq!(all[0].1, "m");
    }

    #[test]
    fn a_configured_price_is_a_starting_point_not_a_permanent_answer() {
        let mut prices: BTreeMap<String, BTreeMap<String, ModelPriceConfig>> = BTreeMap::new();
        prices.entry(P.to_string()).or_default().insert(
            "m".to_string(),
            ModelPriceConfig {
                input: 3.0,
                output: 15.0,
                cache_read: None,
                schedules: Vec::new(),
            },
        );
        let mut table = PriceTable::new();
        table.seed_from_config(&prices);

        // Until something is observed, the configured split is billed.
        assert_eq!(
            table.estimate(P, "m", 0, &usage(1_000_000, 0, 0), Decimal::ONE),
            Some(Decimal::from(3))
        );
        assert_eq!(
            table.entry(P, "m", 0).unwrap().source().map(|s| s.label()),
            Some("configured")
        );

        // After one observation the learned rate takes over for billing...
        table.observe(P, "m", 0, Decimal::from(9), &usage(1_000_000, 0, 0));
        assert_eq!(
            table.entry(P, "m", 0).unwrap().source().map(|s| s.label()),
            Some("learned")
        );
        assert_eq!(
            table.estimate(P, "m", 0, &usage(1_000_000, 0, 0), Decimal::ONE),
            Some(Decimal::from(9))
        );
        // ...while the configured expectation stays put as the reference.
        let entry = table.entry(P, "m", 0).unwrap();
        assert_eq!(
            entry.configured.unwrap().input_per_token,
            Decimal::new(3, 6)
        );
    }

    /// Regression for the trap in blended pricing: identical prices produce very
    /// different per-million figures on different mixes, so the per-million
    /// number is only meaningful next to the mix, while the ratio is what can be
    /// watched over time because it is zero whenever prices match.
    #[test]
    fn the_same_prices_produce_different_per_million_figures_on_different_mixes() {
        let mut prices: BTreeMap<String, BTreeMap<String, ModelPriceConfig>> = BTreeMap::new();
        prices.entry(P.to_string()).or_default().insert(
            "m".to_string(),
            ModelPriceConfig {
                input: 0.30,
                output: 1.20,
                cache_read: Some(0.03),
                schedules: Vec::new(),
            },
        );

        // Input-heavy: 1M prompt (200k of it cache read) + 10k output.
        //   800k x 0.30/M + 200k x 0.03/M + 10k x 1.20/M = 0.258
        let mut heavy_in = PriceTable::new();
        heavy_in.seed_from_config(&prices);
        heavy_in.observe(
            P,
            "m",
            0,
            Decimal::new(258, 3),
            &usage(1_000_000, 10_000, 200_000),
        );

        // Output-heavy: 100k prompt + 200k output.
        //   100k x 0.30/M + 200k x 1.20/M = 0.27
        let mut heavy_out = PriceTable::new();
        heavy_out.seed_from_config(&prices);
        heavy_out.observe(P, "m", 0, Decimal::new(270, 3), &usage(100_000, 200_000, 0));

        let a = heavy_in.drift(P, "m", 0).unwrap();
        let b = heavy_out.drift(P, "m", 0).unwrap();

        // Both are exactly on price, so neither is a repricing.
        assert!(a.ratio.abs() < 0.02, "got {:+.1}%", a.ratio * 100.0);
        assert!(b.ratio.abs() < 0.02, "got {:+.1}%", b.ratio * 100.0);
        assert!(!a.significant && !b.significant);

        // Yet their per-million figures are nowhere near each other.
        assert!(
            b.actual_per_million > a.actual_per_million * Decimal::from(2),
            "same prices: {} vs {}",
            a.actual_per_million,
            b.actual_per_million
        );
        let (input_pct, out_pct, cache_pct) = heavy_in
            .entry(P, "m", 0)
            .unwrap()
            .mix_percentages()
            .unwrap();
        assert!(
            (input_pct - 79.2).abs() < 0.1
                && (out_pct - 1.0).abs() < 0.1
                && (cache_pct - 19.8).abs() < 0.1,
            "the mix is what explains the difference, got {input_pct}/{out_pct}/{cache_pct}"
        );
    }

    #[test]
    fn a_repricing_shows_up_in_the_ratio_whatever_the_mix_is() {
        let mut prices: BTreeMap<String, BTreeMap<String, ModelPriceConfig>> = BTreeMap::new();
        prices.entry(P.to_string()).or_default().insert(
            "m".to_string(),
            ModelPriceConfig {
                input: 0.30,
                output: 1.20,
                cache_read: Some(0.03),
                schedules: Vec::new(),
            },
        );
        // 100k input tokens at the configured $0.30/M would be $0.03; the
        // observed spend says $0.06, i.e. the input price has doubled to $0.60/M.
        let mut t = PriceTable::new();
        t.seed_from_config(&prices);
        t.observe(P, "m", 0, Decimal::new(6, 2), &usage(100_000, 0, 0));
        let d = t.drift(P, "m", 0).unwrap();
        assert!(
            (d.ratio - 1.0).abs() < 0.01,
            "a doubling must read as +100%, got {:+.1}%",
            d.ratio * 100.0
        );
        assert!(d.significant);
        assert_eq!(d.expected_per_million, Decimal::new(30, 2));
        assert_eq!(d.actual_per_million, Decimal::new(60, 2));
    }

    /// The point of tiers: the same request costs different amounts depending on
    /// when it starts. If the gate priced every request at the default rate, a
    /// peak request would be under-estimated by the peak multiplier -- the
    /// direction that lets a window overflow into paid credits.
    #[test]
    fn a_windowed_price_charges_the_peak_tier_at_its_own_rate() {
        use crate::config::ScheduleConfig;
        let mut cfg: BTreeMap<String, BTreeMap<String, ModelPriceConfig>> = BTreeMap::new();
        cfg.entry(P.to_string()).or_default().insert(
            "m".to_string(),
            ModelPriceConfig {
                input: 0.15,
                output: 0.60,
                cache_read: Some(0.003),
                schedules: vec![ScheduleConfig {
                    name: Some("peak".into()),
                    days: "mon-fri".into(),
                    hours: "01:00-04:00,06:00-10:00".into(),
                    tz: Some("UTC".into()),
                    input: 0.30,
                    output: 1.20,
                    cache_read: Some(0.006),
                }],
            },
        );

        let mut table = PriceTable::new();
        assert_eq!(table.seed_from_config(&cfg), 2, "default plus peak");

        let million = usage(1_000_000, 0, 0);
        assert_eq!(
            table.estimate(P, "m", 0, &million, Decimal::ONE),
            Some(Decimal::new(15, 2)),
            "the default tier"
        );
        assert_eq!(
            table.estimate(P, "m", 1, &million, Decimal::ONE),
            Some(Decimal::new(30, 2)),
            "the peak tier, exactly twice the default"
        );
    }

    /// Peak and off-peak are measured independently, so a rise in one does not
    /// masquerade as a rise in the other. That is what a pooled figure cannot do.
    #[test]
    fn each_tier_carries_its_own_drift() {
        use crate::config::ScheduleConfig;
        let mut cfg: BTreeMap<String, BTreeMap<String, ModelPriceConfig>> = BTreeMap::new();
        cfg.entry(P.to_string()).or_default().insert(
            "m".to_string(),
            ModelPriceConfig {
                input: 0.15,
                output: 0.60,
                cache_read: Some(0.003),
                schedules: vec![ScheduleConfig {
                    name: Some("peak".into()),
                    days: "mon-fri".into(),
                    hours: "01:00-04:00".into(),
                    tz: None,
                    input: 0.30,
                    output: 1.20,
                    cache_read: Some(0.006),
                }],
            },
        );
        let mut table = PriceTable::new();
        table.seed_from_config(&cfg);

        let million = usage(1_000_000, 0, 0);
        // Default tier charges exactly what was configured.
        table.observe(P, "m", 0, Decimal::new(15, 2), &million);
        // Peak tier charges 30% more than configured, which is above the
        // reporting threshold.
        table.observe(P, "m", 1, Decimal::new(39, 2), &million);

        let off = table.drift(P, "m", 0).unwrap();
        let peak = table.drift(P, "m", 1).unwrap();
        assert!(
            off.ratio.abs() < 0.01,
            "off-peak unchanged, got {:+.1}%",
            off.ratio * 100.0
        );
        assert!(
            (peak.ratio - 0.30).abs() < 0.01,
            "peak up 30%, got {:+.1}%",
            peak.ratio * 100.0
        );
        // Only the drifted tier is reported.
        let drifted = table.drifted();
        assert_eq!(drifted.len(), 1);
        assert_eq!(drifted[0].2, 1, "the peak tier");
    }

    #[test]
    fn prices_are_scoped_per_provider() {
        let mut table = PriceTable::new();
        // 2 USD/M and 20 USD/M: same shape as before, but priced like a model
        // rather than like the old ceiling test that let anything through.
        table.observe("cheap", "m", 0, Decimal::new(2, 3), &usage(1_000, 0, 0));
        table.observe("dear", "m", 0, Decimal::new(2, 2), &usage(1_000, 0, 0));
        assert_ne!(
            table.get("cheap", "m", 0).unwrap().input_per_token,
            table.get("dear", "m", 0).unwrap().input_per_token,
            "the same model name must not share one price across providers"
        );
        assert_eq!(table.known_models(), 2);
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

    /// Regression, seen against live providers. Both xKiro and Command Code
    /// buffer short completions into a single burst, and dividing by that
    /// near-zero window produced figures like 14818 and 77170 tokens per
    /// second. An unmeasurable window must report nothing rather than a number
    /// that looks like data.
    #[test]
    fn tps_is_none_for_a_buffered_single_burst() {
        let t0 = Instant::now();
        // All 64 tokens arrived in one read, 1ms after the first content frame.
        assert_eq!(
            compute_tps(64, t0, t0 + std::time::Duration::from_millis(1)),
            None
        );
        // A window long enough to measure but still implying impossible speed.
        assert_eq!(
            compute_tps(500, t0, t0 + std::time::Duration::from_millis(120)),
            None,
            "4166 tok/s is not a real generation rate"
        );
        // A genuinely streamed response is measured normally.
        let tps = compute_tps(100, t0, t0 + std::time::Duration::from_secs(1)).unwrap();
        assert!((tps - 100.0).abs() < 0.001, "got {tps}");
        // No tokens means no rate.
        assert_eq!(
            compute_tps(0, t0, t0 + std::time::Duration::from_secs(1)),
            None
        );
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
        table.observe(P, "m", 0, Decimal::new(1, 2), &usage(2_000, 0, 0)); // 0.000005/token
        assert_eq!(
            table.get(P, "m", 0).unwrap().input_per_token,
            Decimal::new(5, 6)
        );

        let cost = table
            .estimate(P, "m", 0, &usage(1_000, 1_000, 1_000), Decimal::ONE)
            .unwrap();
        // (0 uncached + 1000 cached * 0.1 + 1000 completion) * 5e-6 = 0.0055
        assert_eq!(cost, Decimal::new(55, 4));
        // A learned rate already carries the account's billing unit, so a
        // multiplier does not change it -- see the multiplier test above.
        assert_eq!(
            table
                .estimate(P, "m", 0, &usage(1_000, 1_000, 1_000), Decimal::from(2))
                .unwrap(),
            cost
        );
    }
}
