# 09: Apply provider and account changes

**What to build:** Provider and account edits apply to the running pool without dropping work. Adding an Account puts it in the pool as unknown and probes it asynchronously; removing one retires it (no new work, in-flight attempts settle, ledger rows stay); editing base URL, currency, multiplier or limits updates the live Account; changing a provider's adapter or base URL resets that provider's Accounts to unknown and re-probes them. Kept Accounts keep gates, readings, sticky bindings, in-flight commitments and pacing windows.

**Blocked by:** 08 (Apply a policy change to the running gateway).

- [ ] Adding an Account makes it appear as unknown and it serves nothing until a probe succeeds.
- [ ] Removing an Account takes it out of selection immediately; an attempt already in flight settles, and its ledger rows remain.
- [ ] Editing an Account's config-derived fields changes the next attempt's behaviour; sticky bindings and readings survive.
- [ ] Changing a provider's adapter or base URL resets its Accounts to unknown and queues re-probes.
- [ ] A failed apply of these sections changes nothing in the pool or the file.
- [ ] Both provider and account sections are editable in the form and apply through the same path as policy.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
