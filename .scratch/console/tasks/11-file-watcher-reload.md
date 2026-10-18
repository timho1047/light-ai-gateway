# 11: Reload external edits through the file watcher

**What to build:** Editing the configuration file in an editor now takes effect the same way a console save does. The watcher notices the file (including a rename-based save), submits it to the same serialised apply path, and a save made by the console itself does not cause a second apply. An invalid external edit keeps the running snapshot and is reported in the log and on the configuration page; the operator can fix the file and it applies again.

**Blocked by:** 09 (Apply provider and account changes), 10 (Apply route, alias and price changes).

- [ ] An external edit to the file is applied through the same engine, with the same live/restart-only rules.
- [ ] A console save does not trigger a second apply through the watcher.
- [ ] An invalid external edit keeps the previous snapshot serving and surfaces the failure on the configuration page and in the log.
- [ ] Fixing the file afterwards applies it without a restart or a console action.
- [ ] The watcher does not apply a file whose content is unchanged.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
