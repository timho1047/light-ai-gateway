# 05: Complete the Board

**What to build:** The Board answers the operator's first question in full: per-Account Covered windows with used and cap, Metered balance, reset countdowns, stale, disabled and parked detail with the last error and return time, pool inventory per window, vitals, and the price-drift summary. It refreshes every 10 seconds while the tab is visible and pauses when hidden; a Refresh control re-reads immediately; a fresh ledger renders 'not probed yet', never an empty pool.

**Blocked by:** 01 (Split the CLI into per-command modules), 04 (Console entry shell and minimal Board).

- [ ] Every field above renders from the live pool snapshot and is marked stale/not-stale as the Reading is.
- [ ] The auto-refresh partial updates the Board without a full page load and stops polling in a hidden tab.
- [ ] `lgw status` output is byte-identical and now shares its data assembly with the Board.
- [ ] A never-probed ledger and an empty tail render explanatory copy, not empty tables.
- [ ] Both languages render the same fields and values.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
