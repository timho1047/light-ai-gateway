# 12: Usage page with ledger indexes

**What to build:** The Usage page answers spend and traffic questions from the ledger: Covered and Metered spend over time, token counts and cache-hit rate, request and failure counts, ranges of 24 hours, 7, 30 and 90 days and all, grouping by provider, model, account or day, read-only connections only, exact Decimal totals kept apart by currency, and a note that latency history stops at 30 days. The ledger gains only the indexes this needs; existing rows and the money format do not change.

**Blocked by:** 01 (Split the CLI into per-command modules), 04 (Console entry shell and minimal Board).

- [ ] Usage renders Covered and Metered separately against the selected range and grouping.
- [ ] Money totals are exact, per currency, and match the ledger; a missing window is omitted rather than zero.
- [ ] Latency figures stop at 30 days and the page says why.
- [ ] The usage page makes no provider request and takes only read-only ledger connections.
- [ ] `lgw usage` output is byte-identical and now shares its aggregation with the page.
- [ ] The schema version advances with index creation only; existing databases migrate without rewriting rows.
- [ ] Both languages render the same numbers; charts have text summaries.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
