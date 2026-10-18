# 07: Validate a draft and show the diff

**What to build:** The configuration page can submit a draft: the server applies it to the existing document (comments, key order and unknown keys preserved), validates the whole configuration with the same rules every command uses, and returns field-level errors rendered in the page's language plus the TOML diff of the change. Nothing is written to disk in this slice.

**Blocked by:** 06 (Read-only configuration page).

- [ ] A valid draft returns the TOML diff of exactly what would change.
- [ ] An invalid draft answers 422 with the field-level error, rendered in the requested language from a stable error code; the CLI still prints its existing English message unchanged.
- [ ] Comments, key order and unknown keys survive the document edit.
- [ ] No code path in this slice writes the configuration file.
- [ ] Both languages render the same errors and diff.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
