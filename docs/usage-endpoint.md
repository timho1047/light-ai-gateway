# GET /v1/usage

**Status:** implemented, in `src/quota_report.rs` and the `usage` handler in
`src/proxy.rs`. This file is the contract those two are held to: the paths below
are public, and a widget template pinned to them breaks if they move.

**English** | [简体中文](usage-endpoint.zh-CN.md)

A read of the pool's last-known quota, shaped so a JSON-path client — in
particular [DeepSeek-Balance-Whale-Widget](https://github.com/MeteorNOX/DeepSeek-Balance-Whale-Widget)
— can show rolling windows and a remaining balance without learning this
gateway's account model.

The numbers are the same ones `lgw status` already has. The work is
projection, not a new probe.

## Why this exists

The gateway pools several provider accounts behind one `local_key`. The whale
treats one API key as one vendor account and reads a GET with
`Authorization: Bearer {key}`, then picks fields out of the JSON by path.

Those two views only meet if this endpoint:

1. Speaks the whale's existing parsers (`quota.json.windows` and `balance.json`).
2. Aggregates a pool into the three window names the whale already renders
   (`rolling` / `weekly` / `monthly`) plus one remaining-money figure.
3. Never invents a zero, never mixes currencies, and never returns a live
   provider key.

A second URL, a second token, or a whale-specific `/api/whale/*` is a second
contract. Do not add one.

## The consumer contract

The whale's host (`fetchModelQuota` / `fetchModelBalance` in
`DeepSeek-Balance-Whale-Widget/lib/index.js`) is a JSON-path client, not a
schema client. Compatibility is these rules, not a documented OpenAPI file.

| Rule | Why |
|---|---|
| Method is GET | `apiFetchJson` never POSTs |
| `Authorization: Bearer <token>` | the whale's default `auth: 'Bearer {key}'` |
| HTTP 200 on success | any other status is thrown as `HTTP {n}` |
| `percent` is **used** 0–100, not remaining | OpenCode Go and the whale both treat it as used |
| `resetsAt` is RFC 3339, or a unix timestamp (seconds if `< 1e12`, else ms) | `apiPlanResetMs` |
| Multi-window fields are **named paths**, not `windows[i]` | the template cannot iterate |
| Window keys the UI already understands: `rolling`, `weekly`, `monthly` | `BUBBLE_PLAN_WIN_OPTS` |
| A 200 body must not look like a business error | `ok:false`, `success:false`, `error.message`, or a numeric `code` other than 0/200 plus `msg` all fail the parse |

Quota URLs come only from the whale **template** (`tpl.quota`). A model row
cannot override them. The built-in `custom` template has `balance` only, so
windows need a `kind: 'quota'` template (see Whale template section).
Balance and quota may hit the same URL; the whale will GET it twice.

Loopback and LAN URLs are allowed. The token the whale stores is this
gateway's `local_key`, never an upstream account key.

## Endpoint

    GET /v1/usage
    GET /v1/usage?provider=<provider-id>
    GET /v1/usage?account=<provider>/<account-id>

Register it next to `/v1/models` on the same axum router, behind the same
`require_local_key` middleware.

### Query

| Parameter | Meaning |
|---|---|
| `provider` | Restrict the snapshot to that provider id (a `[providers.*]` key). |
| `account` | Restrict to one account. Must be the **qualified** id (`xkiro/acc-1`). A bare `acc-1` is rejected: account ids are unique only within a provider. |

If both are set, the account must belong to that provider; otherwise 404.

The whale will not invent query parameters. Put the filter in the template
URL (`{base}/v1/usage?provider=xkiro`). One whale model per provider is the
intended pairing.

### Auth

Identical to `/v1/chat/completions` and `/v1/models`:

- `Authorization: Bearer <local_key>` when `server.local_key` is set.
- No `local_key` configured: the port is open, same warning as today.
- `local_key` configured but unreadable: refuse every request (fail closed).

Do not accept an upstream account key here. This endpoint talks about the
pool, and the pool is what the local key already unlocks.

## Response (200)

Two layers in one object.

- `usage` and `balance` are the whale projection. Their paths are frozen.
- `accounts` is the native view, aligned with `lgw status`. The whale ignores
  unknown fields; this layer can grow.

`plan` is NOT part of the first implementation. `ProbeOutcome.plan` is
currently discarded by `Pool::apply_probe`. Do not invent a plan name, and do
not expand the pool schema just to fill this field.

```json
{
  "object": "usage",
  "checked_at": "2026-04-08T12:04:11Z",
  "stale": false,
  "currency": "USD",
  "usage": {
    "rolling": {
      "percent": 47.5,
      "resetsAt": "2026-04-08T16:30:00Z"
    },
    "weekly": {
      "percent": 22.0,
      "resetsAt": "2026-04-13T00:00:00Z"
    },
    "monthly": {
      "percent": 18.5,
      "resetsAt": "2026-05-01T00:00:00Z"
    }
  },
  "balance": {
    "remaining": 5.00,
    "used": 0,
    "total": 5.00,
    "currency": "USD"
  },
  "accounts": [
    {
      "id": "xkiro/acc-1",
      "provider": "xkiro",
      "label": "acc-1",
      "state": "active",
      "currency": "USD",
      "stale": false,
      "last_probe_at": "2026-04-08T12:04:11Z",
      "windows": {
        "rolling": {
          "used": "9.50",
          "cap": "20.00",
          "resetsAt": "2026-04-08T16:30:00Z"
        },
        "weekly": {
          "used": "17.60",
          "cap": "80.00",
          "resetsAt": "2026-04-13T00:00:00Z"
        },
        "monthly": {
          "used": "37.00",
          "cap": "200.00",
          "resetsAt": "2026-05-01T00:00:00Z"
        }
      },
      "pots": [
        {
          "kind": "wallet",
          "remaining": "5.00",
          "used": "0"
        }
      ]
    }
  ]
}
```

### Top-level fields

| Field | Meaning |
|---|---|
| `object` | Always `"usage"`. |
| `checked_at` | RFC 3339. Latest `last_probe_at` among accounts that contributed. |
| `stale` | `true` if any contributing gate is stale. |
| `currency` | Only when every contributed money amount shares one currency. Omit otherwise. |
| `usage` | Only when the scope projects cleanly (see Aggregation). |
| `usage.<window>` | Omit if no usable gate. **Never** emit `percent: 0` as a stand-in. |
| `usage.<window>.percent` | JSON number. Used × 100, rounded to 1 decimal, clamped 0–100. |
| `usage.<window>.resetsAt` | RFC 3339. Omit if none. |
| `balance` | Only when at least one metered pot in one currency. |
| `accounts` | Always present. A DTO. **Never** serde of `domain::Account` (that type holds `ApiKey`). |

### Window key map

| Whale key | `GateKind` | Notes |
|---|---|---|
| `rolling` | `FiveHour` | Covered. |
| `weekly` | `Weekly` | Covered. |
| `monthly` | `Monthly` | Covered. |
| — | `FreeTokens` | Not a whale window. Omit from `usage` and from `accounts[].windows`. |
| — | `Rpm` | Not shown. |

The same map applies to `usage` and to `accounts[].windows`.

### Account object

| Field | Meaning |
|---|---|
| `id` | `qualified_id` (`xkiro/acc-1`). |
| `provider` | The `[providers.*]` key. |
| `label` | CLI-friendly name. |
| `state` | snake_case `AccountState`: `active`, `draining`, `parked`, `disabled`, `unknown`. |
| `currency` | This account's currency. |
| `stale` | `true` if any of this account's gates is stale. |
| `last_probe_at` | RFC 3339. |
| `last_error` | Optional. Already redacted. |
| `available_at` | Optional. When the account is parked. |
| `windows` | Named object, not an array. Same keys as the whale map. Omit a key that has no usable gate. |
| `windows.<k>.used` | Decimal text. |
| `windows.<k>.cap` | Decimal text. |
| `windows.<k>.resetsAt` | RFC 3339. Omit if none. |
| `pots` | Metered only (`Wallet`, `PurchasedCredits`). Empty array if none. |
| `pots[].kind` | snake_case `GateKind`: `wallet`, `purchased_credits`. |
| `pots[].remaining` | Decimal text. |
| `pots[].used` | Decimal text. |

Do not emit `base_url`, `api_key`, or secret refs.

## Aggregation

Top-level `percent` is **inventory**:

    round1(100 * sum(used) / sum(cap))

over matching covered gates with `cap > 0`, including parked and disabled.
No gates ⇒ contribute nothing (omit the window). `sum(cap) == 0` ⇒ omit
the window.

`resetsAt`: earliest among exhausted or parked gates of that kind; else
earliest of that kind; else omit. Exhausted means `used >= cap` (remaining
is zero) on a covered gate of that kind.

`balance`: `Metered` only.

- `remaining` = `sum(remaining)`
- `used` = `sum(used)`
- `total` = `remaining + used`

Do not add covered remaining into `balance`. Omit `balance` if currencies
mix.

Emit `usage` only when all of these hold:

1. At least one covered money window (`FiveHour` / `Weekly` / `Monthly`)
   with `cap > 0`.
2. One currency across the amounts that would feed that projection.
3. A single provider, **or** every contributed provider shares that same
   currency for that window.

Unfiltered multi-provider / multi-currency: omit `usage` (and omit
`balance` if currencies disagree). Keep `accounts`.

A never-probed scope is **503**, not an empty projection. "Never probed"
means the scope has accounts and none of them has a successful probe
(`last_probe_at` set and gates held). A 200 with `stale: true` is the
other case: the last probe failed but previous gates were kept.

Disabled accounts still contribute their held gates. A 200 is correct
when every account is disabled but gates exist.

Do not live-probe on this request.

## Errors

Non-200 responses use the existing `error_response` envelope:

```json
{
  "error": {
    "message": "...",
    "type": "<kind>",
    "code": "<kind>"
  }
}
```

A 200 body must never contain `error.message`, `ok:false`, or
`success:false`.

| Status | When |
|---|---|
| 401 | Missing or wrong bearer, or `local_key` is configured but unreadable. Same `invalid_api_key` path as the other routes. |
| 404 | Unknown `provider`, unknown `account`, or `account` does not belong to the given `provider`. |
| 400 | `account` is not a qualified id (`provider/account-id`). A bare `acc-1` is 400, not 404. |
| 503 | The scope has accounts but none has been successfully probed. |
| 200 | `stale: true` if the last probe failed but gates were held. |
| 200 | All accounts disabled, but gates exist. |

Do not live-probe to recover from 503.

## Must not

- No provider calls from this handler.
- No client-supplied URL used as a probe host.
- No serde of `domain::Account`.
- No invented zeros (`percent: 0` as a stand-in, empty windows filled in).
- No mixing covered remaining into `balance`.
- No mixing currencies or providers into one `percent`.
- No cookies, and no upstream account keys in the body.
- No `/v1/dashboard/billing/*` in the same change.

## Implementation sketch

- `src/usage.rs`: a pure function `usage_report` from a snapshot plus the
  query filter to the response DTO (or an error).
- `ProxyBackend::snapshot()`: the last-known accounts. The handler reads
  memory; it does not probe.
- `proxy.rs`: `GET /v1/usage` on the same router as `/v1/models`, behind
  `require_local_key`.
- `accounts[]` money fields are Decimal **text**, not `f64`.

Tests, named as behaviours:

- `pool_used_percent_is_inventory_not_the_next_account`
- `a_parked_account_still_counts_in_the_pool_percent`
- `covered_remaining_is_not_added_to_balance`
- `mixed_currencies_omit_the_top_level_projection`
- `a_missing_window_is_omitted_not_zero`
- `a_stale_contributor_marks_the_report_stale`
- `never_probed_scope_is_unavailable`
- `account_filter_requires_a_qualified_id`
- `the_report_does_not_contain_an_api_key`

HTTP test: `local_key` required; a 200 body has no `error` object.

`apply_probe` does not need to change.

## Whale template

JS template `light_ai_gateway` with `kind: 'quota'`, `needsBaseUrl`,
quota URL `{base}/v1/usage?provider=xkiro`, windows `rolling` / `weekly` /
`monthly`, balance on the same URL, `probeUrl` `{base}/v1/models`. The
credential the whale stores is this gateway's `local_key`.

```javascript
const light_ai_gateway = {
  id: 'light_ai_gateway',
  kind: 'quota',
  needsBaseUrl: true,
  auth: 'Bearer {key}',
  probeUrl: '{base}/v1/models',
  quota: {
    url: '{base}/v1/usage?provider=xkiro',
    json: {
      windows: {
        rolling:  { percent: 'usage.rolling.percent',  resetsAt: 'usage.rolling.resetsAt' },
        weekly:   { percent: 'usage.weekly.percent',   resetsAt: 'usage.weekly.resetsAt' },
        monthly:  { percent: 'usage.monthly.percent',  resetsAt: 'usage.monthly.resetsAt' },
      },
    },
  },
  balance: {
    url: '{base}/v1/usage?provider=xkiro',
    json: {
      remaining: 'balance.remaining',
      used: 'balance.used',
      total: 'balance.total',
      currency: 'balance.currency',
    },
  },
};
```

Until that template exists, custom HTTP can only bind `balance`.

## Acceptance

`curl` with `Authorization: Bearer <local_key>`. Seven checks:

1. The three windows render (`rolling` / `weekly` / `monthly`).
2. `balance.remaining` works.
3. One exhausted account plus one fresh account in the same pool shows
   about **50%**, not 100% (inventory, not "the next account").
4. A stale contributor keeps the held numbers (`stale: true`, no zeros).
5. 401 without a bearer; 503 on a never-probed scope; no 200 body contains
   an `error` object.
6. `accounts` matches `lgw status` for the same scope.
7. The body contains no `api_key` field and no `sk-` secret.

## Later

Not first PR:

- Persist `plan` (`ProbeOutcome.plan` is discarded today).
- OneAPI billing pair (cents).
- Ledger per-model spend.

## Frozen paths

These are public once a whale template ships:

- `usage.rolling.percent`
- `usage.*.resetsAt`
- `balance.remaining`

Additive fields are fine. Renames are breaking.
