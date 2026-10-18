# 04: Console entry shell and minimal Board

**What to build:** Running `lgw serve` now also serves a browser console on the same listener. The root address shows the console shell: navigation, the Board with one row per Account (provider, label, state, Covered summary, Metered), a light default with a night switch remembered per browser, and English/Simplified Chinese copy with the switch carried across links. Access is loopback-only, responses carry the security headers, no cookie is set, and the console URL is logged at startup.

**Blocked by:** 02 (Split the HTTP layer from the Gateway).

- [ ] Every console path answers 404 to a non-loopback peer, independent of `local_key`.
- [ ] A foreign `Host`, a foreign `Origin` on a mutation, or a non-JSON mutation content type is refused and changes nothing.
- [ ] Responses carry a restrictive content security policy, no framing, nosniff and no-store; no `Access-Control-*` header is ever emitted.
- [ ] The Board rows come from the live pool snapshot and rendering makes no provider request.
- [ ] Night mode survives navigation; `?lang=zh` renders Chinese and links carry the parameter; no cookie.
- [ ] Assets are embedded in the binary, with a development override that serves them from disk.
- [ ] Startup logs the console URL at the listening address.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
