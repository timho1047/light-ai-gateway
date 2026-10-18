# 01: Split the CLI into per-command modules

**What to build:** The same `lgw` binary, with each subcommand's implementation in its own module and the configuration-path resolution, the colour decision and the table helpers in one shared home. This is a mechanical move: no behaviour, output, exit code or flag changes. It exists so that later slices can lift a command's rules into an operations layer used by both the CLI and the console.

**Blocked by:** None (can start immediately).

- [ ] Every existing CLI test passes with no change to its assertions.
- [ ] Each subcommand is implemented in its own module; shared path, colour and table code has one home.
- [ ] `lgw --help`, output bytes and exit codes are unchanged for every command.
- [ ] `cargo test`, `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` pass.
