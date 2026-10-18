# light-ai-gateway

**English** | [简体中文](README.zh-CN.md)

A small OpenAI-compatible gateway that pools multiple provider accounts and routes around exhausted quota windows, so you stop switching API keys by hand.

Point any OpenAI client at it, keep your normal model names, and it picks an account per request, switches when a window fills, and records what everything cost.

![The operator console: accounts, covered allowance, metered balance and live inventory](docs/assets/console-board.png)

*A demo console; the listener port in your config may differ.*

**Contents**

- [Why](#why)
- [Install](#install)
- [Quick start](#quick-start)
- [How it works](#how-it-works)
- [Configuration](#configuration)
- [Console and operations](#console-and-operations)
- [Troubleshooting](#troubleshooting)
- [Docs](#docs)
- [Development](#development)
- [Safety](#safety)
- [License](#license)

## Why

- **OpenAI Chat Completions.** One endpoint, your existing SDK, no code change.
- **Pool accounts per provider.** Same provider, same model, several keys; the gateway chooses one per request.
- **Switch before you notice.** Accounts near the soft threshold stop taking new sessions; requests move to the next account and come back when the window resets.
- **Never burn metered money by accident.** Paid balance stays behind an explicit per-provider opt-in.
- **See what happened.** The terminal board and the browser console show allowance, reset countdowns, cost, tokens, cache hit rate and recent attempts.
- **Configure it in the browser.** The console edits the same config file and publishes changes without dropping in-flight requests. Loopback only, no login.

## Install

Requires Rust **1.88 or newer**. The credential store uses macOS Keychain, Windows Credential Manager or the freedesktop Secret Service; only macOS needs the signing step below.

```
cargo install --path . --locked
lgw --version
```

That builds release binaries for `lgw` and `lgwl` into `~/.cargo/bin`.

**On macOS, use the wrapper instead:**

```
scripts/lgw-identity.sh      # once: creates the signing certificate
scripts/lgw-install.sh       # cargo install, then signs the binaries
```

A plain `cargo install` replaces the signed binary with an ad-hoc one, and the keychain starts asking for your password again. Skip the wrapper only if the prompts do not bother you.

Other ways to get the binary: `cargo build --release` and copy `target/release/lgw`, or `cargo install --git <repo-url> --locked`. Remove both binaries with `cargo uninstall light-ai-gateway`.

After pulling changes, run `lgwl update`. It stops a running gateway, reinstalls, and restarts it with the same arguments; `--no-restart` leaves it stopped. See [the launcher contract](docs/launcher.md).

## Quick start

```
lgw init                                      # writes ~/.light-ai-gateway/config.toml
$EDITOR ~/.light-ai-gateway/config.toml        # fill in providers, accounts and routes
lgw keys add xkiro acc-1                      # hidden prompt; once per account
lgw keys add xkiro acc-2
lgw keys list                                 # check every credential resolves

lgw doctor                                    # config, credentials, probe reachability
lgwl launch                                   # proxy + console in the background
lgwl status                                   # is it running?
lgwl log                                      # follow its log; Ctrl-C stops only the viewer
```

The generated config is already a working example. The part you must edit has this shape:

```
[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"
currency = "USD"

[providers.xkiro.accounts.acc-1]

[routes]
"z-ai/glm-5.3-flash" = "xkiro"
```

An account with no `key` reads its credential from the OS store, under `<provider>/<account>`. Route keys are the model ids sent upstream.

Point a client at the proxy:

```
export OPENAI_BASE_URL=http://127.0.0.1:8787/v1
export OPENAI_API_KEY=unused          # or the local_key you configured
```

```
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer unused' \
  -d '{"model":"z-ai/glm-5.3-flash","messages":[{"role":"user","content":"hello"}]}'
```

You should get a normal Chat Completions response. The same address serves the console: open <http://127.0.0.1:8787/>.

`lgw serve` is still available when you want the log in the foreground. `lgwl exit` stops the background instance.

## How it works

An **account** is one provider credential with its own quota state. Quota arrives as **windows** (for example 5-hour, weekly or monthly), each with a cap and a reset time. The gateway tracks every account's windows and decides per request which account may serve it.

There are two kinds of money, kept deliberately separate:

| Class | Examples | Behaviour |
|---|---|---|
| **Covered** | subscription spend windows, monthly plan credits | Prepaid and forfeited when the window resets. Preferred. |
| **Metered** | wallet balance, purchased credits, API balance | Real money, permanent once spent. Refused unless the operator opts in. |

The opt-in is `[policy] allow_metered` for everyone, with `[providers.<name>] allow_metered` overriding it per provider.

Account selection, in one breath:

1. **Hard gate.** Only accounts whose covered headroom covers the estimated cost plus a reserve are candidates.
2. **Sticky first.** Keep the session on its current account when it is still a candidate; moving it destroys the provider's prompt cache.
3. **Consume-first.** Below the soft threshold, use the most-used account so switches are rare.
4. **Soft threshold is a preference, not a wall.** If every account is above 80%, the least-used one still serves while headroom remains.

When the pool is empty, the gateway answers 429 with a real Retry-After from the earliest reset. It never silently falls through to another provider or spends metered balance. The full rules, including failure handling, are in [routing](docs/routing.md) and [operations](docs/operations.md).

## Configuration

Resolution order: `--config PATH`, then `$LGW_CONFIG`, then `./lgw.toml`, then `~/.light-ai-gateway/config.toml`. `lgw doctor` prints which file it used.

```
~/.light-ai-gateway/config.toml       configuration
~/.light-ai-gateway/gateway.db        usage ledger (SQLite)
~/.light-ai-gateway/lgwl/             launcher pid/log state
the OS credential store               provider API keys
```

Keys are never stored in the file by default: `lgw keys add` puts them in the OS store, and the config refers to them by provider/account name. A local token protects the listener when it is reachable beyond loopback; set `server.local_key` to require it from clients. Details in [credentials](docs/credentials.md).

The console's form applies changes to the running process without dropping in-flight requests. Settings that only take effect on restart (`listen`, `db_path`, `max_body_bytes`) are shown as restart-only rather than silently ignored. The console never rewrites comments or keys it does not understand. The full access and reload contract is in [the console docs](docs/web-ui.md).

## Console and operations

The console is served by the same process and listener as the proxy:

- Board: accounts, covered allowance, metered balance, resets, inventory and price drift.
- Usage: totals by provider/model/account, daily chart, TTFT and cache hit rate.
- Requests: recent attempts, retries, filters and coverage.
- Prices, Keys, Doctor and Config: the CLI's operator actions plus the config form.
- Loopback only, no login, no cookie, no session. Mutating requests also need a JSON body and a same-origin header.

Common commands:

| Command | What it does |
|---|---|
| `lgw serve` | Run the proxy and console in the foreground |
| `lgwl launch` / `lgwl exit` | Run it in the background / stop it |
| `lgw status` | Account board with reset countdowns |
| `lgw doctor` | Config, credentials, probe reachability and common misconfigurations |
| `lgw keys add/list/remove/test` | Manage provider credentials |
| `lgw accounts list/probe/disable/enable` | Inspect and recover accounts |
| `lgw usage --since 7d --group-by provider,model` | Usage report |
| `lgw prices [--drift-only]` | Configured versus learned prices |
| `lgw tail` | Recent requests |

`lgw doctor` is the first thing to run when something is wrong. The launcher, logging and process lifecycle are documented in [docs/launcher.md](docs/launcher.md); day-to-day recovery and output reading are in [docs/operations.md](docs/operations.md).

## Troubleshooting

| Symptom | First look |
|---|---|
| `no eligible account` on every request | `lgw status`: accounts may be parked, disabled or under the hard gate. The 429 body names the account with the most room. |
| An account stays DISABLED after a fixed key | A probe restores only an account with headroom; an operator disable needs `lgw accounts enable`. |
| A password prompt on every run (macOS) | The grant is bound to the code signature and `cargo install` creates an ad-hoc one; run `scripts/lgw-install.sh`. |
| A price looks wrong | `lgw prices --drift-only`. A learned rate is an observation; a configured price is never overwritten by one. |
| Every request is 429 with a long Retry-After | A covered window is spent; `lgw status` shows the reset countdown. |

More symptoms and fixes: [operations](docs/operations.md).

## Docs

- [The console contract](docs/web-ui.md)
- [The background launcher](docs/launcher.md)
- [HTTP API](docs/api.md)
- [Credentials and the macOS keychain](docs/credentials.md)
- [Prices, learned rates and drift](docs/prices.md)
- [Routing and the two kinds of quota](docs/routing.md)
- [Operations and troubleshooting](docs/operations.md)
- [GET /v1/usage contract](docs/usage-endpoint.md)
- [Architecture decision records](docs/adr/)

## Development

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

There is no CI workflow: those three plus a release build are the pre-change check. Unit tests live beside the code they cover; the CLI lifecycle test is in `tests/lgwl_cli.rs`.

Provider quota adapters live in `src/quota`. Adding a provider means implementing one trait and declaring its base URL, credentials and error vocabulary. `adapter = "none"` cannot serve traffic: an account joins the pool only after a successful quota probe, so use `xkiro`, `command-code`, `deepseek` or `haoai`.

## Safety

- Binds to loopback by default. A non-loopback listener requires a local key; the config is refused without one rather than left to a warning.
- Provider keys live in the OS credential store, are never logged, and are redacted on every debug/display path.
- Probe hosts are pinned in config and redirects are refused, so a request cannot redirect a credential elsewhere.
- A failed quota probe keeps the previous reading as stale; it never resets it to zero.
- Prompt and response bodies are not stored.
- Console mutations need JSON plus a same-origin header and are checked against `Host` and `Origin`.
- Config writes are validated as a whole and replaced atomically; a file edited underneath the console is never overwritten.

## License

MIT. See [LICENSE](LICENSE). Use it, modify it, ship it, sell it. It comes with no warranty, and it spends real money when you configure it to.
