//! Price learning: what one probe interval says a model costs.
//!
//! Two probes bracket an interval. Inside it the gateway counted the tokens it
//! served; between them the provider's own windows moved by some amount of
//! money. Attributing that money to those tokens is how a per-model rate is
//! learned from real numbers instead of maintained by hand, and the three rules
//! that keep the attribution honest all live here:
//!
//! 1. ACCOUNTS SUM; WINDOWS INSIDE ONE ACCOUNT DO NOT. Two accounts are two
//!    separate pools of money, but a weekly window and the 5-hour window nested
//!    inside it report the same cumulative spend. Measured live on a Command
//!    Code account, both rose by exactly 0.036638504 credits over one interval
//!    while summing them reported 0.073277008 -- exactly double, and half of it
//!    invented. A learned price is a RATE, so every figure derived from that
//!    interval was inflated by the same factor.
//! 2. AN ACCOUNT SEEN FOR THE FIRST TIME CONTRIBUTES NOTHING. Its whole
//!    historic spend would otherwise be charged to whatever tokens happened to
//!    pass through since it first appeared.
//! 3. SPEND WITH NO TOKENS IS DROPPED, NOT SMEARED. It means the account is
//!    being used somewhere other than this gateway, which is exactly when a
//!    learned rate stops meaning anything. The running total is kept, because a
//!    single interval looks like rounding and a day's worth is the answer to
//!    "where did the money go".

use crate::domain::{GateKind, TokenUsage};
use crate::tier::TierKey;
use rust_decimal::Decimal;
use std::collections::HashMap;

/// One account's window readings at the last probe.
///
/// Keyed by gate kind, and that level is load-bearing: see rule 1.
#[derive(Debug, Clone, Default)]
struct SpendSample {
    by_gate: HashMap<GateKind, Decimal>,
}

/// What one probe interval observed.
#[derive(Debug, Clone)]
pub struct Interval {
    /// Money that left the provider's windows during this interval.
    pub spend: Decimal,
    /// What that spend is charged to, grouped by model, each entry carrying the
    /// price tier the tokens were served at. Empty when nothing can be
    /// attributed.
    pub tokens: HashMap<String, Vec<(TierKey, TokenUsage)>>,
    /// Set when spend moved and no local tokens could be attributed to it: this
    /// provider's running total, including this interval.
    pub unattributed: Option<Decimal>,
}

/// The tokens served since the last probe, and the spend each interval moved.
///
/// One lock instead of three: the pending tokens, the last probe's readings and
/// the unattributed totals are only meaningful together, and splitting them
/// across separate fields let a caller update one without the others.
#[derive(Debug, Default)]
pub struct IntervalLedger {
    /// (provider, model, window) -> measured tokens since the last probe.
    pending: HashMap<(String, String, TierKey), TokenUsage>,
    /// Canonical account id -> its window readings at the last probe.
    last_spend: HashMap<String, SpendSample>,
    /// Provider -> spend that moved with no tokens to attribute it to.
    unattributed: HashMap<String, Decimal>,
}

impl IntervalLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count tokens the gateway actually served.
    ///
    /// Only real measurements. An estimate built from "characters divided by
    /// four" is not a token count, and dividing a real spend delta by it
    /// produces a rate that is neither observed nor configured -- one that then
    /// sizes the hard gate for every later request.
    pub fn observe(&mut self, provider: &str, model: &str, tier: TierKey, usage: &TokenUsage) {
        if !usage.measured {
            return;
        }
        let entry = self
            .pending
            .entry((provider.to_string(), model.to_string(), tier))
            .or_default();
        entry.prompt_tokens += usage.prompt_tokens;
        entry.completion_tokens += usage.completion_tokens;
        entry.cached_tokens += usage.cached_tokens;
        entry.reasoning_tokens += usage.reasoning_tokens;
    }

    /// Fold in one probe's readings and report the interval.
    ///
    /// `readings` is each account's covered, non-stale window totals as the probe
    /// has just seen them. Tokens counted since the previous probe are consumed
    /// by this call whenever there is spend to charge them to.
    pub fn settle(
        &mut self,
        provider: &str,
        readings: &[(String, HashMap<GateKind, Decimal>)],
    ) -> Interval {
        let spend = self.interval_spend(readings);
        for (account, gates) in readings {
            self.last_spend.insert(
                account.clone(),
                SpendSample {
                    by_gate: gates.clone(),
                },
            );
        }

        if spend <= Decimal::ZERO {
            // Nothing moved, so there is nothing to attribute. Tokens counted in
            // this interval stay pending for the next one rather than being
            // charged to spend that never happened.
            return Interval {
                spend,
                tokens: HashMap::new(),
                unattributed: None,
            };
        }

        let tokens = self.drain_by_model(provider);
        if tokens.is_empty() {
            let total = self.add_unattributed(provider, spend);
            return Interval {
                spend,
                tokens,
                unattributed: Some(total),
            };
        }
        Interval {
            spend,
            tokens,
            unattributed: None,
        }
    }

    /// Money that moved since the last probe.
    ///
    /// Each account contributes the largest increase any single one of its
    /// windows saw, and accounts add. An account with no previous sample
    /// contributes nothing (rule 2); a window that rolled over inside the
    /// interval reports a drop and is ignored.
    fn interval_spend(&self, readings: &[(String, HashMap<GateKind, Decimal>)]) -> Decimal {
        let mut delta = Decimal::ZERO;
        for (account, gates) in readings {
            let Some(previous) = self.last_spend.get(account).map(|s| &s.by_gate) else {
                continue;
            };
            let largest = gates
                .iter()
                .filter_map(|(kind, used)| previous.get(kind).map(|before| *used - *before))
                .filter(|increase| *increase > Decimal::ZERO)
                .max();
            if let Some(increase) = largest {
                delta += increase;
            }
        }
        delta
    }

    /// Take this provider's pending tokens, grouped by model.
    ///
    /// Every provider's own tokens are consumed, never another's.
    fn drain_by_model(&mut self, provider: &str) -> HashMap<String, Vec<(TierKey, TokenUsage)>> {
        let keys: Vec<(String, String, TierKey)> = self
            .pending
            .keys()
            .filter(|(owner, _, _)| owner == provider)
            .cloned()
            .collect();
        let mut by_model: HashMap<String, Vec<(TierKey, TokenUsage)>> = HashMap::new();
        for key in keys {
            if let Some(usage) = self.pending.remove(&key) {
                let (_, model, tier) = key;
                by_model.entry(model).or_default().push((tier, usage));
            }
        }
        by_model
    }

    /// Add this interval's unattributable spend to the provider's running total.
    fn add_unattributed(&mut self, provider: &str, delta: Decimal) -> Decimal {
        let entry = self
            .unattributed
            .entry(provider.to_string())
            .or_insert(Decimal::ZERO);
        *entry += delta;
        *entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(mantissa: i64, scale: u32) -> Decimal {
        Decimal::new(mantissa, scale)
    }

    fn gates(pairs: &[(GateKind, Decimal)]) -> HashMap<GateKind, Decimal> {
        pairs.iter().copied().collect()
    }

    fn usage(prompt: u64) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: 0,
            cached_tokens: 0,
            reasoning_tokens: 0,
            measured: true,
        }
    }

    /// Regression against a live Command Code account.
    ///
    /// One interval moved the 5-hour window and the weekly window containing it
    /// by exactly 0.036638504 credits each, while the monthly window did not
    /// move at all. Summing the three reported 0.073277008 -- double the money
    /// that actually left the account -- and because a learned price is a rate,
    /// every figure derived from it was inflated by the same factor. It is what
    /// turned 47 tokens into 1559 USD per million.
    #[test]
    fn nested_windows_are_counted_once_not_once_each() {
        let before = gates(&[
            (GateKind::FiveHour, d(1_328_079_011, 9)),
            (GateKind::Weekly, d(4_390_359_400, 9)),
            (GateKind::Monthly, d(4_399_661_385, 9)),
        ]);
        let after = gates(&[
            (GateKind::FiveHour, d(1_364_717_515, 9)),
            (GateKind::Weekly, d(4_426_997_904, 9)),
            (GateKind::Monthly, d(4_399_661_385, 9)),
        ]);
        let mut ledger = IntervalLedger::new();
        // First sight of the account establishes the baseline, not an interval.
        let first = ledger.settle("commandcode", &[("commandcode/acc-1".into(), before)]);
        assert_eq!(first.spend, Decimal::ZERO);

        let interval = ledger.settle("commandcode", &[("commandcode/acc-1".into(), after)]);
        assert_eq!(
            interval.spend,
            d(36_638_504, 9),
            "the same spend seen by two nested windows is still one spend"
        );
    }

    /// Accounts add; windows inside one account do not; a reset does not go
    /// negative; an account seen for the first time contributes nothing.
    #[test]
    fn spend_adds_across_accounts_but_not_within_one() {
        let mut ledger = IntervalLedger::new();
        ledger.settle(
            "p",
            &[
                (
                    "p/a".into(),
                    gates(&[
                        (GateKind::FiveHour, d(1_000, 3)),
                        (GateKind::Weekly, d(2_000, 3)),
                    ]),
                ),
                ("p/c".into(), gates(&[(GateKind::FiveHour, d(2_000, 3))])),
            ],
        );

        let interval = ledger.settle(
            "p",
            &[
                (
                    "p/a".into(),
                    gates(&[
                        // The short window rolled over inside the interval, so
                        // its reading dropped. Only the weekly window saw the
                        // spend.
                        (GateKind::FiveHour, d(10, 3)),
                        (GateKind::Weekly, d(2_800, 3)),
                    ]),
                ),
                // Never probed before: its entire history must not be charged
                // to whatever tokens happen to have passed through.
                ("p/b".into(), gates(&[(GateKind::FiveHour, d(9_000, 3))])),
                ("p/c".into(), gates(&[(GateKind::FiveHour, d(2_250, 3))])),
            ],
        );

        // p/a: max(+800 from weekly, -990 ignored) = 0.800; p/c: +0.250.
        assert_eq!(interval.spend, d(1_050, 3));
    }

    /// Spend with no tokens accumulates, so the warning can report a total and
    /// not just the interval that happens to be small.
    #[test]
    fn unattributed_spend_accumulates_per_provider() {
        let mut ledger = IntervalLedger::new();
        let cc = |used: Decimal| {
            vec![(
                "commandcode/acc-1".to_string(),
                gates(&[(GateKind::FiveHour, used)]),
            )]
        };
        let xk =
            |used: Decimal| vec![("xkiro/a".to_string(), gates(&[(GateKind::FiveHour, used)]))];

        // The first reading of an account is a baseline, not an interval.
        ledger.settle("commandcode", &cc(d(1_000, 3)));
        let first = ledger.settle("commandcode", &cc(d(1_031, 3)));
        assert!(first.tokens.is_empty(), "there were no local tokens");
        assert_eq!(first.unattributed.unwrap(), d(31, 3));

        let second = ledger.settle("commandcode", &cc(d(1_050, 3)));
        assert_eq!(
            second.unattributed.unwrap(),
            d(50, 3),
            "the running total carries the earlier interval"
        );

        // One provider's leak is not another's.
        ledger.settle("xkiro", &xk(d(10, 3)));
        let other = ledger.settle("xkiro", &xk(d(12, 3)));
        assert_eq!(other.unattributed.unwrap(), d(2, 3));

        let third = ledger.settle("commandcode", &cc(d(1_051, 3)));
        assert_eq!(
            third.unattributed.unwrap(),
            d(51, 3),
            "commandcode's total is untouched by xkiro's"
        );
    }

    /// Nothing moved: there is no interval spend to learn from, and the tokens
    /// counted in it wait for the interval that does move.
    #[test]
    fn an_unchanged_interval_learns_nothing() {
        let mut ledger = IntervalLedger::new();
        let same = gates(&[(GateKind::FiveHour, d(1_000, 3))]);
        ledger.settle("p", &[("p/a".into(), same.clone())]);
        ledger.observe("p", "m", TierKey::Default, &usage(500));

        let interval = ledger.settle("p", &[("p/a".into(), same)]);
        assert_eq!(interval.spend, Decimal::ZERO);
        assert!(interval.tokens.is_empty());
        assert!(interval.unattributed.is_none());
    }

    /// Measured tokens are charged to the spend; an estimate is not a
    /// measurement and must not become one.
    #[test]
    fn only_measured_tokens_are_attributed() {
        let mut ledger = IntervalLedger::new();
        ledger.settle(
            "p",
            &[("p/a".into(), gates(&[(GateKind::FiveHour, d(1, 3))]))],
        );
        ledger.observe("p", "m", TierKey::Default, &usage(100));
        let mut estimated = usage(999);
        estimated.measured = false;
        ledger.observe("p", "m", TierKey::Default, &estimated);

        let interval = ledger.settle(
            "p",
            &[("p/a".into(), gates(&[(GateKind::FiveHour, d(2, 3))]))],
        );
        let charged = interval.tokens.get("m").expect("the measured tokens");
        assert_eq!(charged.len(), 1);
        assert_eq!(charged[0].1.prompt_tokens, 100);
    }
}
