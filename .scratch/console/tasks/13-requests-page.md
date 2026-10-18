# 13: Requests page

**What to build:** The Requests page shows recent attempts with time, status, provider, account, model, tokens, TTFT, TPS, coverage and cost, with a limit control, error and coverage filters, and an empty state. `lgw tail` shares the data assembly and its terminal output stays byte-identical.

**Blocked by:** 01 (Split the CLI into per-command modules), 04 (Console entry shell and minimal Board).

- [ ] Attempt-level rows render with the fields above and honestly reflect errors and retries.
- [ ] Limit, status and coverage controls filter the rows.
- [ ] An empty ledger renders explanatory copy, not an empty table.
- [ ] `lgw tail` output is byte-identical and now shares its data assembly with the page.
- [ ] Both languages render the same rows.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
