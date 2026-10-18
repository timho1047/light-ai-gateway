# 08: Apply a policy change to the running gateway

**What to build:** Applying a policy change works end to end. The operator edits a live field, sees the diff, applies: the file is replaced atomically as 0600, the running process publishes a new snapshot and updates pool policy values in place, new requests use them immediately, and a streaming request that started before the apply finishes on its old snapshot and settles with its old price. A draft built from an older file fingerprint is refused with 409; a draft that changes a restart-only field is refused by name; a failed validation changes nothing. `lgw init` uses the same atomic writer.

**Blocked by:** 03 (Introduce the Runtime snapshot and request-scoped configuration), 07 (Validate a draft and show the diff).

- [ ] Policy fields are editable in the form; Apply writes the file atomically with mode 0600 and the reread file matches the draft.
- [ ] A streaming request in flight during an apply completes against its original snapshot; a request started after the apply uses the new values.
- [ ] A stale fingerprint answers 409 and leaves the newer file untouched; a restart-only change is refused with a code.
- [ ] A validation or snapshot-build failure leaves the file and the running snapshot unchanged.
- [ ] Two applies submitted together end in one consistent configuration, not an interleaving.
- [ ] `lgw init` refuses to leave a partial file and creates the file with mode 0600.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
