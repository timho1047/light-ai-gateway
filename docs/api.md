# HTTP API

**English** | [简体中文](api.zh-CN.md)

The proxy surfaces exposed by the gateway. The full /v1/usage contract lives
in [usage-endpoint.md](usage-endpoint.md).

## Endpoints

    GET  /                        the operator console (loopback only)
    POST /v1/chat/completions     the proxy: retries, account switching, the usage ledger
    GET  /v1/models               the models this gateway will serve, described by the provider
    GET  /v1/usage                how much of the pool's allowance is gone, and when it comes back

## Authentication

The /v1 endpoints use the bearer token configured as server.local_key. When it is set, clients must send Authorization: Bearer <token>; the token is never forwarded upstream. The console is not behind this token: it is restricted to loopback peers. The OpenAI client should point at http://127.0.0.1:8787/, the same address as the console.

`/v1/models` answers with your `[routes]` and nothing else: a model the provider
sells but you have not routed is left out, because asking for it gets a "no route
for model" error. The fields come from the provider's own `/v1/models` response,
passed through unchanged -- context length, prices, capability flags -- with `id`,
`object` and, when the provider does not name an owner itself, `owned_by` set to
the provider:

    curl http://127.0.0.1:8787/v1/models                     # every route
    curl "http://127.0.0.1:8787/v1/models?provider=xkiro"    # one provider's routes

One upstream call per provider per request, made with an eligible account's key;
a model list spends no tokens. A provider whose catalog cannot be read does not
break the list: its routes are still returned, without that provider's fields, and
the reason is logged. A route the provider's catalog does not mention is still
listed -- the route is what works -- and `RUST_LOG=debug` prints the published ids
closest to it, which is how a wrong vendor prefix gets found
(`glm/glm-5.3-flash` where xkiro publishes `z-ai/glm-5.3-flash`).

`/v1/usage` is for a dashboard: what the poller last saw, turned into
percentages. It calls no provider, so a widget can refresh on a timer without
spending anything. The windows are named rather than indexed (`rolling`,
`weekly`, `monthly`) so a client that can only follow a JSON path can read them,
and a window with no reading is left out rather than sent as `0`:

    curl -H "Authorization: Bearer $KEY" http://127.0.0.1:8787/v1/usage
    curl -H "Authorization: Bearer $KEY" "http://127.0.0.1:8787/v1/usage?provider=xkiro"
    curl -H "Authorization: Bearer $KEY" "http://127.0.0.1:8787/v1/usage?account=xkiro/acc-1"

The percentages are inventory across the whole scope -- sum(used) over sum(cap),
parked accounts included -- so one spent account beside one fresh one reads 50%,
not 0%. `usage` and `balance` carry JSON numbers, for drawing; the same figures
under `accounts[]` keep their exact decimals as text, for auditing. Covered
allowance never lands in `balance`, because a subscription's headroom is not
money you can spend anywhere else. A scope nobody has probed yet answers 503:
zeros and an untouched allowance look identical. The full shape is in
[usage-endpoint.md](usage-endpoint.md).
