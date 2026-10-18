# 03: Introduce the Runtime snapshot and request-scoped configuration

**What to build:** The same behaviour with one structural change: at startup the process builds one immutable Runtime snapshot of everything configuration-derived -- resolved routes and aliases, provider views, policy values and timeouts, configured price seeds, schedules, the probe set, the upstream client configuration and the resolved local-key state. Each request and each probe captures the snapshot once at admission and uses it to settlement, so a later apply can publish a new snapshot without touching work already running.

**Blocked by:** 02 (Split the HTTP layer from the Gateway).

- [ ] Request handling reads configuration from a snapshot captured at admission; estimate, tier and settlement use that same snapshot.
- [ ] The backend interface no longer lends a reference to a gateway-owned configuration.
- [ ] A test publishes a second snapshot while a streaming request is in flight: that request completes on the first snapshot and a request started afterwards uses the second.
- [ ] Startup behaviour with one snapshot is unchanged; the poller and probes capture the same way.
- [ ] `cargo test`, `clippy --all-targets -- -D warnings` and `fmt --check` pass.
