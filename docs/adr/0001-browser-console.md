# The console is a loopback-only operator surface

**English** | [简体中文](0001-browser-console.zh-CN.md)

The README used to list a web UI as a non-goal. Editing configuration files and
running CLI commands was the only way to change routing, prices and policy, and
that was deliberate for a process that holds paid credentials. We now ship a
browser console in the same binary, because changing configuration and reading
usage are what an operator does most, and the file is the slowest part. The
non-goal is narrowed rather than removed: the console is loopback-only, has no
login, cookie or session, and leaves the `/v1` contract untouched. It is one
operator's tool, not a second client API and not a multi-user admin panel.

## Considered Options

- A terminal UI: no browser surface at all, but no charts and nothing usable
  while the terminal is busy. Still the better tool if the operator never leaves
  the machine and never wants a browser.
- A separate companion service: it would duplicate configuration parsing,
  credential access and ledger reads, and break the one-binary install promise.

## Consequences

- `README.md` no longer lists a web UI as out of scope, and the access and
  same-origin rules that keep the console from becoming an open door are in
  `docs/web-ui.md`.
- The console is a public surface and needs its own tests for Host, Origin and
  loopback checks (see `docs/web-ui.md`).
