# 16: Keys page

**What to build:** The Keys page lists every configured Account's credential source (credential store, environment, file, literal present) and lets the operator add, test and remove a provider key. Keys are write-only to the operating system's credential store, are never returned by any response or log, and the page works against an in-memory credential backend in tests. `lgw keys` shares the operations and keeps byte-identical output.

**Blocked by:** 01 (Split the CLI into per-command modules), 04 (Console entry shell and minimal Board).

- [ ] The page lists each Account's source and never a value; the raw TOML view stays redacted.
- [ ] Adding a key writes only to the credential store, testing it probes the provider, removing it deletes only the stored credential.
- [ ] No page, fragment, response header or log line contains a key value.
- [ ] The actions require the console's JSON and custom-header guards.
- [ ] `lgw keys` output is byte-identical and shares the operations.
- [ ] Both languages render the same fields.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
