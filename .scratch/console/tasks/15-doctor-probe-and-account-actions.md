# 15: Doctor, probe and account actions

**What to build:** The console exposes the CLI's operational actions: doctor with all checks and failures in both languages, probing one or all Accounts, disabling an Account with a confirmation, and enabling it back (which returns it to unknown until a probe confirms it). Network actions run inside the request with a running state, a bounded timeout and a sanitised provider error; a probe never undoes an operator disable. `lgw doctor` and `lgw accounts` share the operations and their output is unchanged.

**Blocked by:** 01 (Split the CLI into per-command modules), 04 (Console entry shell and minimal Board).

- [ ] Doctor reports every check and failure with the same verdicts as the CLI, rendered in the requested language.
- [ ] Probe one and probe all reach the configured providers through the mock upstream in tests and update the Board afterwards.
- [ ] Disable asks for confirmation and is not undone by a probe; enable returns the Account to unknown until a probe proves it.
- [ ] A slow or failing provider produces a timeout or a sanitised error fragment, not a hung page.
- [ ] The CLI's doctor and accounts output is byte-identical and shares the operations.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
