# 14: Prices page and forget action

**What to build:** The Prices page shows configured and learned prices per provider, model and tier with the observed mix and drift, and offers forgetting a learned rate (all, one provider, one model) with a confirmation. The action is a guarded mutation, writes only the ledger, and the page refreshes to show the configured price back in force. `lgw prices` shares the data assembly and keeps byte-identical output.

**Blocked by:** 01 (Split the CLI into per-command modules), 04 (Console entry shell and minimal Board).

- [ ] Configured vs learned prices, mix, drift and samples render per provider, model and tier.
- [ ] Forgetting one provider, one model or everything asks for confirmation, performs the same ledger write as the CLI, and refreshes the page.
- [ ] The action requires the console's JSON and custom-header guards and a foreign page cannot trigger it.
- [ ] `lgw prices` output is byte-identical and now shares its data assembly with the page.
- [ ] Both languages render the same numbers.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
