# An apply is atomic, and running requests keep their snapshot

**English** | [简体中文](0003-apply-is-atomic.zh-CN.md)

A request reads configuration at several points: admission, retry planning, price
tier, timeouts, and the price that settles the attempt. A change that lands
between two of those reads would settle a request with a price that did not admit
it, or route an attempt through a provider that no longer exists. An apply
therefore publishes a new immutable runtime snapshot and leaves the old one alive
until the last request holding it settles. Live pool state -- gates, sticky
bindings, in-flight commitments, pacing windows -- is reconciled in place rather
than rebuilt: a `Reservation` holds a clone of the pool, so swapping the pool
would settle real spend into a discarded object that never reaches the ledger.
Validation and building the replacement snapshot happen before the file is
written; failure changes nothing. Settings that would rewire the process
(`listen`, `db_path`, `max_body_bytes`) are restart-only and are refused by name.

## Considered Options

- Drain before applying: simpler, but a reload could stall for as long as traffic
  continues, which is exactly when it is needed.
- Swap the whole `Gateway`: reservations would settle against the old pool while
  the new one serves the next request, so commitments and spend diverge.

## Consequences

- The HTTP layer reads configuration through a request-scoped snapshot rather
  than a reference to a gateway field; the gateway gains interior mutability at
  the seams a snapshot does not cover (prices, probes, schedules, pool policy).
- A removed account is retired: it takes no new work and disappears after its
  last attempt settles; its ledger rows stay.
