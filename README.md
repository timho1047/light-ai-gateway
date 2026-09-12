# light-ai-gateway

A very small OpenAI-compatible gateway that pools multiple provider accounts and
routes around exhausted quota windows, so you stop switching API keys by hand.

Point any OpenAI client at it, keep using your normal model names, and it will
pick an account for each request, switch when a window fills up, and record what
everything cost.

## What it does

- **Speaks OpenAI Chat Completions.** One endpoint, your existing SDK, no code change.
- **Pools accounts per provider.** Same provider, same model, several keys.
- **Switches before you notice.** Accounts at 80% stop taking new sessions;
  requests move to the next account and come back when a window resets.
- **Refuses to burn real money.** Paid-credit and wallet spend is fenced off
  behind an explicit opt-in.
- **Records usage.** Input, output, cached and reasoning tokens, TTFT, TPS, cost,
  cache hit rate, per attempt.
- **Shows you the numbers.** A terminal board of covered allowance against
  metered balance, with reset countdowns.

Design and rationale: [docs/DESIGN.md](docs/DESIGN.md).

## Quick start

    cargo build --release
    ./target/release/light-ai-gateway init          # writes an example config
    cp config.example.toml ~/.light-ai-gateway/config.toml
    $EDITOR ~/.light-ai-gateway/config.toml         # add providers, accounts, routes

    ai-gw keys add xkiro acc-1                      # hidden prompt; run once per account
    ai-gw keys add xkiro acc-2
    ai-gw keys list                                 # check everything resolves

    ai-gw doctor                                    # config, credentials, probe reachability
    ai-gw serve

Then point your client at it:

    export OPENAI_BASE_URL=http://127.0.0.1:8787/v1
    export OPENAI_API_KEY=unused          # or the local_key you configured

## Credentials

Keys go into the operating system's own credential store (macOS Keychain,
Windows Credential Manager, freedesktop Secret Service) via the keyring crate.
Accounts in the config need no key field at all; omitting it looks the account up
by provider/account name.

    ai-gw keys add <provider> <account>     # hidden prompt, never an argv flag
    ai-gw keys add <provider> <account> --stdin
    ai-gw keys list                         # source and status, never the secret
    ai-gw keys test <provider> <account>    # prove it by probing the provider
    ai-gw keys remove <provider> <account>

The key is read from a hidden prompt rather than a command-line argument, because
arguments are visible to every process on the machine and land in shell history.

Explicit alternatives are still supported when you want them:
key = { keychain = true }, key = { env = "X" }, key = { key_file = "/run/secrets/x" },
or an inline literal (discouraged).

### What this actually protects

- **Protected:** other users on the machine, backups and disk images, accidental
  commits, and anything that reads files but not the credential store.
- **NOT protected:** a process already running as you. It can ask the OS for the
  same secret; macOS unlocks the login keychain for the session.

A vault file encrypted with a key compiled into the binary would *look* like it
covered that second case and would not: whoever has the binary has the key. That
is obfuscation rather than encryption, so it is deliberately not implemented. If
you need the credentials to travel between machines, export them from the OS
store rather than committing a ciphertext.

Set AI_GW_CREDENTIAL_BACKEND=memory to force an ephemeral store, which is useful
for tests and dry runs and never touches your real keychain.

## Commands

    ai-gw serve                          start the proxy
    ai-gw status [--refresh]             account board
    ai-gw usage --since 7d --group-by provider,model,account
    ai-gw accounts list|probe|disable
    ai-gw tail                           recent requests
    ai-gw doctor                         config, credentials, probe reachability
    ai-gw init                           write an example config

## The two pots of money

Every quota check is one of two kinds, and the gateway never conflates them:

| Class | Examples | Behaviour |
|---|---|---|
| **Covered** | xKiro 5h/7d spend windows, Command Code monthly credits, free token allowance | Prepaid by a subscription. Forfeited when the window resets. Preferred. |
| **Metered** | xKiro wallet, Command Code purchased credits, DeepSeek balance | Real money. Permanent once spent. Refused unless the operator opts in. |

The distinction is not cosmetic. Command Code does not return an error when a
5-hour window is exhausted; it silently bills your purchased credits instead, so
the request succeeds while your balance drops. A gateway that only reacts to 429s
will spend money without ever telling you. The hard gate below is the guard.

## How an account is chosen

For each request:

1. **Hard gate.** Only accounts whose covered headroom covers the estimated cost
   plus a reserve (the p95 of recent request costs) are candidates. This is what
   stops a burst from overflowing into paid credits.
2. **Sticky first.** If the session is already bound to a candidate, keep it.
   Moving a conversation discards the provider's prompt cache and re-bills the
   whole history at full input price.
3. **Consume-first.** Among candidates below the soft threshold, take the most
   used. Draining one account before moving on minimises switches, and every
   switch costs a cache miss. (Measured over a simulated 72 hours: consume-first
   produced 27 switches where least-used-first produced about 1400.)
4. **Soft threshold is a preference, not a wall.** If every account is above 80%
   the least-used one is still used. A threshold you configured must not become
   an outage while headroom genuinely remains.
5. **All accounts unavailable.** HTTP 429 with a real Retry-After, taken from the
   earliest window reset. It does not silently fall through to another provider,
   and it does not silently spend metered balance.

## Failure handling

A 429 means several different things and each needs a different response:

| Signal | Meaning | Action |
|---|---|---|
| 429 with a short Retry-After | pacing | retry the **same** account; keep the cache warm |
| 429 with a long Retry-After, or a quota-shaped code | window exhausted | park the account, move on |
| 402 / insufficient_quota | out of money | disable the account, alert; never retry |
| 401 / 403 | bad credentials | disable the account |
| 529 / 5xx | upstream trouble | bounded retry, then another account |
| 400 / 404 | bad request | never retried |

An account switch is only possible **before the first byte reaches the client**.
Once a response has started streaming it is relayed as-is; a streamed failure is
reported rather than retried, because a retried stream can be billed twice.

## Metrics

TTFT is measured to the first **content** token, skipping role-only and preamble
chunks. TPS excludes TTFT from its denominator. Every upstream attempt is stored
separately, so retries and account switches do not pollute the timings.

Cache hit rate (cached divided by prompt tokens) is the cost metric to watch: it
is what account switching destroys.

## Non-goals

Cross-provider fallback, cross-model downgrade, protocol translation, semantic
caching, multi-user RBAC and a web UI are all deliberately out of scope. A model
maps to exactly one provider; changing that is a product decision, not something
the gateway should infer.

## Development

    cargo test              # unit and integration tests
    cargo clippy --all-targets

Provider quota adapters live in src/quota. Adding a provider means implementing
one trait and declaring its base URL, credentials and error vocabulary.

## Safety notes

- Binds to loopback by default and holds paid credentials; set a local key if
  anything else can reach the port.
- Keys come from the environment or a dedicated file, are never logged, and are
  redacted in every debug or display path.
- Probe hosts are pinned in configuration, so a credential cannot be redirected
  to an address supplied with a request.
- A failed quota probe keeps the previous reading and marks it stale. It never
  resets a reading to zero, which would both overstate availability and let the
  CLI claim a funded account is empty.
- Prompt and response bodies are never stored.
