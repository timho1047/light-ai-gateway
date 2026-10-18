# 06: Read-only configuration page

**What to build:** A configuration page that shows what the gateway is actually running: the resolved file path, its content fingerprint, the last applied time, a structured read-only view of every known section (policy, providers, accounts, routes, prices), a live/restart-only marker on every field, and a read-only raw TOML view whose literal secrets are redacted. Nothing on the page writes.

**Blocked by:** 04 (Console entry shell and minimal Board).

- [ ] The page shows the resolved path, the fingerprint of the file it read, and the last applied time.
- [ ] Every known section and field is rendered; live and restart-only fields are marked differently.
- [ ] The raw TOML view redacts literal secrets, and no response ever carries the raw file bytes.
- [ ] The page renders in both languages with the same values.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
