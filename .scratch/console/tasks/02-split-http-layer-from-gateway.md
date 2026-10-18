# 02: Split the HTTP layer from the Gateway

**What to build:** The same `/v1` routes and behaviour, with the HTTP layer separated from the Gateway: one module assembles the router, authentication middleware, body limit and listener; the proxy handlers, the frozen usage handler and the service wiring live in their own modules. The Gateway keeps the pool, prices, probes, ledger writer and interval ledger. This is preparation, not behaviour: existing integration tests move with the code.

**Blocked by:** None (can start immediately).

- [ ] The application presents exactly the same routes, authentication responses, body limit and error envelopes as before.
- [ ] Existing HTTP integration tests are moved, not rewritten, and pass.
- [ ] Adding a route later requires touching one assembly point, not the proxy handler module.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
