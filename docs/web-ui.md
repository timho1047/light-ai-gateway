# The console

**Status:** the contract for the console, held by `src/http/console.rs`,
`src/console/` and `src/runtime.rs`: who can reach it, what it may change, and
when a change takes effect. It is not the `/v1` contract; that one does not move.

**English** | [简体中文](web-ui.zh-CN.md)

## What it is

The console is the operator's surface served by `lgw serve`: the Board, the usage
report, recent requests, every action the CLI has, and an editor for
`config.toml`. It is served by the same process and the same listener as the
proxy, at `/`.

It is one operator's tool, not a client API and not a multi-user admin panel.
There is no login, no account, no cookie and no session. The only thing that can
reach it is the machine the gateway runs on.

## Access

| Surface | Path | Who can reach it |
|---|---|---|
| Console pages | `GET /`, `/usage`, `/requests`, `/prices`, `/config`, `/keys`, `/doctor` | loopback peers only |
| Embedded assets | `GET /console/assets/*` | loopback peers only |
| Console API | `/console/api/*` | loopback peers only |
| OpenAI-compatible API | `/v1/*` | unchanged: the `local_key` bearer rules |

A request whose peer address is not loopback gets 404 from every console path,
whether or not a `local_key` is configured. Serving on a non-loopback address
still lets the console in through `127.0.0.1`; it does not expose it to the
network.

The console does not use `server.local_key` and never accepts an upstream account
key. It is not behind the `/v1` bearer middleware.

### Same-origin defences

- Mutating console requests must be `application/json` and carry
  `X-LGW-Console: 1`.
- `Host` must be `127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>` with
  the listener's port.
- A request that carries `Origin` must have a loopback origin.
- The console never emits `Access-Control-*` headers, so a cross-origin
  preflight fails and a form POST from another page cannot reach a mutating
  endpoint.
- Every console response carries `Content-Security-Policy: default-src 'self'`,
  `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff` and
  `Cache-Control: no-store`.
- Reads have no side effects; changing anything is a POST.

### Responses

Pages are server-rendered HTML. The Board polls an HTML fragment from
`/console/partials/board`; mutations answer small result payloads the page's own
script consumes (HTML for draft validation and Apply, JSON for account, price and
credential actions). There is no JSON admin API and no machine-readable surface
for third-party clients.

The console uses no cookie at all. Language and theme are remembered in the browser
(a URL parameter plus `localStorage`), never as server state. The browser is not a boundary between the operator and their own machine,
but any page the operator visits can send requests to loopback. The
`Origin`/`Host`/custom-header triple is what keeps that page out; the loopback
peer check keeps every other host out.

## Parity with the CLI

Everything the CLI does, except starting itself and writing a fresh example
file, is on the console. The console calls the same operations; it has no second
implementation of a rule.

| CLI | Console |
|---|---|
| `lgw status [--refresh]` | Board; `--refresh` is Probe all on the Doctor page, plus per-account Probe on the Board |
| `lgw usage --since --group-by` | Usage: range and group controls |
| `lgw accounts list/probe/disable/enable` | Board rows: Probe, Disable and Enable |
| `lgw tail --limit` | Requests: time, status, attempt and cost, with a limit control |
| `lgw doctor` | Doctor: every check with a bilingual verdict |
| `lgw keys add/list/remove/test` | Keys: source and status, Store key, Test and Remove |
| `lgw prices`, `--drift-only`, `--forget` | Prices: both rates and drift; Forget all/provider/model |
| `lgw init` | not offered: the gateway already has a configuration |
| editing `config.toml` by hand | Config: draft, Validate, diff, Apply |

Known gaps, all deliberate: `lgw usage` also prints the price-and-drift panel
after the usage table, while the console splits that into the Usage and Prices
pages; `lgw tail` accepts any limit while the console offers 25/50/100/250;
`lgw prices --drift-only` is a filter, while the console sorts significant
drift first rather than hiding the rest; `lgw serve` starts the process and has
no console counterpart because the console is served by it. The sidebar and the
CLI also share the account state machine, the provider probe adapters and the
doctor check assembly; only the persistence target differs, because the console
also has to update the live pool while the terminal writes the ledger row.

Network actions (probe, doctor, key test) run in one request with a progress
state and a result, not as a background job.

## Configuration

`config.toml` is the only source of truth. The console edits that file, not a
copy and not a database:

1. The form edits a draft; nothing is written until Apply.
2. Apply rewrites the draft into the existing TOML document with `toml_edit`, so
   comments, key order and unknown fields survive.
3. The whole document is parsed and validated by the same code every command
   uses; the first error names the field.
4. The replacement runtime snapshot is built (credentials resolved, probes
   prepared) before anything is written.
5. A TOML diff is shown before Apply. The file is replaced atomically (temporary
   file in the same directory, then rename) with mode 0600, and only then is the
   snapshot published.

If the file changed on disk after the form was loaded, Apply answers 409 and asks
for a reload; it never overwrites an edit made in an editor. A change that fails
validation or snapshot construction changes nothing at all.

Edits made outside the console are not ignored. A watcher polls the file's
content and submits new bytes through the same apply rules and the same serialized
path. A file that cannot be loaded keeps the old snapshot and shows the failure on
the Configuration page and in the log; fixing the file applies it without a
restart or a console action. A file written by the console itself is not applied
a second time.

Secrets are not editable values. A key field is shown as its source (credential
store, environment, file, or literal present); adding or rotating a key writes it
to the operating-system credential store only. No response, log or page ever
carries a key.

## What a change does to a running gateway

An Apply publishes a new immutable runtime snapshot. A request keeps the snapshot
it started with until it settles, including its retries; a request that starts
afterwards sees the new one. Nothing is drained and nothing waits.

Live pool state survives: Gates, Readings, sticky session bindings, in-flight
commitments and pacing windows belong to the process, not to the snapshot.

| Changes live | Needs a restart |
|---|---|
| `[policy]` values | `server.listen` |
| providers: adapter, base URLs, currency, limits, metered opt-in | `server.db_path` |
| accounts: add, remove, label, limits | `policy.max_body_bytes` |
| `[routes]` and provider prefix aliases | |
| `[prices]` configured values and schedules | |
| `server.local_key` | |

The listener and body-size fields are shown read-only in the form; a crafted
draft that tries to change one is refused by name. A new account enters `unknown`
and is probed before it can serve. A removed
account is retired: it takes no new work, and its last in-flight attempts settle
against the pool they entered. Changing a provider's adapter or base URL resets
that provider's accounts to `unknown` and re-probes them; a reading that came
from the old endpoint is never kept.

An Apply that fails validation changes nothing: the old snapshot keeps serving
and the console returns the errors.

## Usage data

The Board is the live pool: it reads process memory and never probes a provider.
The Usage page reads the ledger through a read-only connection and never writes.

- Ranges: 24 hours, 7 days, 30 days, 90 days, all.
- Money keeps the ledger's Decimal precision; the console never re-adds it as a
  float.
- Attempt-level detail is pruned after 30 days, so latency figures (TTFT p50/p95,
  TPS) stop at 30 days; spend, tokens and request counts do not.
- The Board re-fetches its fragment every 10 seconds; the other pages load on
  demand and after an action. There is no websocket and no streaming tail.

## Languages

The copy is English and Simplified Chinese. `?lang=zh` selects Chinese and the
default is English; links carry the parameter and the switch in the top bar
changes it. The switch remembers the choice in `localStorage` (never a cookie), and
the theme (light/dark, light by default) is remembered the same way.

## Must not

- No console path answers a non-loopback peer.
- No cookie, session or login.
- No `Access-Control-*` header, ever.
- No credential value in a page, an API response or a log.
- No change to `/v1/*` shape, ordering or authentication.
- No write to `config.toml` that has not been validated as a whole.
- No live probe from a read endpoint.
- No Apply that leaves half of the new configuration in force.
- No background job for an action the CLI runs synchronously.

## Tests

Integration tests, named as the behaviour they assert:

- `a_foreign_host_does_not_see_the_console`
- `the_console_is_not_behind_the_local_key`
- `console_writes_need_json_and_the_console_header`
- `a_stale_apply_leaves_the_newer_file_untouched`
- `an_invalid_draft_is_a_field_error_in_both_languages`
- `a_draft_keeps_comments_order_and_unknown_fields`
- `a_removed_account_retires_after_its_in_flight_attempt_settles`
- `a_published_snapshot_does_not_reprice_an_in_flight_request`
- `a_restart_only_apply_is_refused_by_name_and_writes_nothing`
- `an_invalid_apply_changes_neither_the_file_nor_the_snapshot`
- `an_external_edit_is_applied_once_through_the_watcher`
- `an_invalid_external_edit_keeps_serving_and_surfaces_the_failure`
- `a_console_save_is_not_applied_again_by_the_watcher`
- `the_usage_page_shows_the_same_numbers_in_both_languages`
- `the_requests_page_shows_attempts_and_filters`
- `the_prices_page_shows_both_rates_and_forgets_them`
- `the_doctor_page_reports_every_check_in_both_languages`
- `the_keys_page_lists_sources_and_manages_the_store`
- `probing_and_operator_state_work_through_the_console`
