# 10: Apply route, alias and price changes

**What to build:** Route, alias and price edits apply immediately. A route added or remapped resolves for the next request; a provider alias appears in the snapshot; configured prices are re-seeded without overwriting learned rates; tier schedules replace wholesale and a removed schedule stops applying; removing a route or a configured price takes effect without the old mapping lingering.

**Blocked by:** 08 (Apply a policy change to the running gateway).

- [ ] A route added, remapped or removed takes effect for the next request and never for a request already in flight.
- [ ] Configured prices are re-seeded; learned rates and the interval ledger survive an apply.
- [ ] A schedule added, changed or removed changes the tier charged on the next request.
- [ ] An apply of these sections failing leaves routes and prices as they were.
- [ ] Routes, aliases and prices are editable in the form and apply through the same path as policy.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
