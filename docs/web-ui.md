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
| Console page | `GET /` | loopback peers only |
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

The console uses no cookie at all; its language is a URL parameter, not server
state. The browser is not a boundary between the operator and their own machine,
but any page the operator visits can send requests to loopback. The
`Origin`/`Host`/custom-header triple is what keeps that page out; the loopback
peer check keeps every other host out.

## Parity with the CLI

Everything the CLI does, except starting itself and writing a fresh example
file, is on the console. The console calls the same operations; it has no second
implementation of a rule.

| CLI | Console |
|---|---|
| `lgw status [--refresh]` | Board; Refresh probes first |
| `lgw usage --since --group-by` | Usage controls |
| `lgw accounts list/probe/disable/enable` | Pool rows and per-account actions |
| `lgw tail` | Recent requests |
| `lgw doctor` | Doctor |
| `lgw keys add/list/remove/test` | Keys; list shows a source, never a value |
| `lgw prices`, `--drift-only`, `--forget` | Prices |
| `lgw init` | not offered: the gateway already has a configuration |
| editing `config.toml` by hand | Config |

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

A new account enters `unknown` and is probed before it can serve. A removed
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
- Pages refresh on a timer (10 seconds on the Board, 30 on Usage) and on demand.
  There is no websocket and no streaming tail.

## Languages

The copy is English and Simplified Chinese. `?lang=zh` selects Chinese and the
default is English; links carry the parameter and the switch in the top bar
changes it. Nothing is stored in a cookie.

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

- `the_console_is_loopback_only`
- `a_cross_origin_write_is_refused`
- `a_host_header_from_another_name_is_refused`
- `a_stale_form_does_not_overwrite_the_file`
- `an_invalid_draft_changes_nothing`
- `comments_and_unknown_keys_survive_a_save`
- `a_removed_account_drains_and_keeps_its_ledger_rows`
- `an_in_flight_request_keeps_its_snapshot`
- `a_restart_only_setting_is_refused_by_name`
- `no_console_response_contains_a_key`
- `chinese_and_english_pages_render_the_same_fields`
