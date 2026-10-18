# 17: Amend the contract docs and run the final audit

**What to build:** The console contract document and its Chinese twin describe what shipped: HTML fragment responses instead of a JSON admin API, read-only restart-only fields, language and theme stored in the browser, and the loopback/same-origin rules as implemented. The CLI to console parity table is audited against the real commands; a manual pass covers every page in both themes and both languages; the throwaway mockups are removed; the full verification set passes.

**Blocked by:** 05 (Complete the Board), 06 (Read-only configuration page), 07 (Validate a draft and show the diff), 08 (Apply a policy change to the running gateway), 09 (Apply provider and account changes), 10 (Apply route, alias and price changes), 11 (Reload external edits through the file watcher), 12 (Usage page with ledger indexes), 13 (Requests page), 14 (Prices page and forget action), 15 (Doctor, probe and account actions), 16 (Keys page).

- [ ] The console contract and README (both languages) match the implemented behaviour; no sentence promises a JSON API or a cookie.
- [ ] The parity audit lists every CLI command and its console counterpart, with any gap explained.
- [ ] A manual browser pass covers every page, both themes and both languages, with no console errors and no secret visible.
- [ ] The throwaway mockup directory is deleted from the working tree.
- [ ] `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` and a release build pass.
