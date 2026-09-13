# light-ai-gateway

**English** | [简体中文](README.zh-CN.md)

A very small OpenAI-compatible gateway that pools multiple provider accounts and
routes around exhausted quota windows, so you stop switching API keys by hand.

Point any OpenAI client at it, keep using your normal model names, and it will
pick an account for each request, switch when a window fills up, and record what
everything cost.

**Contents**

- [What it does](#what-it-does)
- [Install](#install)
- [Quick start](#quick-start)
- [Credentials](#credentials)
- [Commands](#commands)
- [Reading the output](#reading-the-output)
- [Account recovery](#account-recovery)
- [Prices](#prices)
- [The two pots of money](#the-two-pots-of-money)
- [How an account is chosen](#how-an-account-is-chosen)
- [Failure handling](#failure-handling)
- [Metrics](#metrics)
- [When it does not work](#when-it-does-not-work)
- [Non-goals](#non-goals)
- [Development](#development)
- [License](#license)
- [Safety notes](#safety-notes)

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

Design and rationale: [docs/DESIGN.md](docs/DESIGN.md) (in Chinese). This file
is the English reference; [README.zh-CN.md](README.zh-CN.md) is the Chinese one.
The two mirror each other -- if they ever disagree, the English one is what the
code does.

## Install

Requires Rust **1.88 or newer** -- declared as `rust-version`, so an older
toolchain is told so plainly instead of failing on a syntax error. The credential
store binds to macOS Keychain, Windows Credential Manager and the freedesktop
Secret Service; only macOS needs the signing step below.

The build produces a single self-contained binary: no runtime, no interpreter,
nothing to install alongside it.

    cargo install --path . --locked

That builds in release mode and puts lgw on your PATH (~/.cargo/bin), which is
already there if you installed Rust with rustup. Check it:

    lgw --version

**On macOS, use the wrapper instead:**

    scripts/lgw-install.sh

It runs the same `cargo install` and then signs the result with a stable identity.
Skip it and the keychain asks for your password once per stored key, on every run,
forever -- see [Credentials](#credentials). The first time you need
`scripts/lgw-identity.sh`, which creates the certificate.

Other ways to get the same binary:

    # build it and copy it wherever you like; it has no dependencies
    cargo build --release && cp target/release/lgw /usr/local/bin/

    # straight from a git remote, once this is pushed somewhere
    cargo install --git <repo-url> --locked

To remove it again:

    cargo uninstall light-ai-gateway

Re-run cargo install after pulling changes; it replaces the binary in place.

## Quick start

    lgw init                                      # writes ~/.light-ai-gateway/config.toml
    $EDITOR ~/.light-ai-gateway/config.toml         # add providers, accounts, routes

    lgw keys add xkiro acc-1                      # hidden prompt; once per account
    lgw keys add xkiro acc-2
    lgw keys list                                 # check everything resolves

    lgw doctor                                    # config, credentials, probe reachability
    lgw serve

Where things live:

    ~/.cargo/bin/lgw                          the binary
    ~/.light-ai-gateway/config.toml             configuration
    ~/.light-ai-gateway/gateway.db              usage ledger (SQLite)
    the OS credential store                     API keys

Config resolution order: --config PATH, then $LGW_CONFIG, then ./lgw.toml,
then ~/.light-ai-gateway/config.toml. lgw doctor prints which one it used.

Then point your client at it:

    export OPENAI_BASE_URL=http://127.0.0.1:8787/v1
    export OPENAI_API_KEY=unused          # or the local_key you configured

### Endpoints

    POST /v1/chat/completions     the proxy: retries, account switching, the usage ledger
    GET  /v1/models               the models this gateway will serve, described by the provider

`/v1/models` answers with your `[routes]` and nothing else: everything the
provider sells but you have not routed is left out, because asking for one of
those gets a "no route for model" error, and a model picker full of models that
cannot be called is worse than a short one. The fields come from the provider's
own `/v1/models` response, passed through unchanged -- context length, prices,
capability flags, whatever that provider publishes -- with `id`, `object` and,
when the provider does not name an owner itself, `owned_by` set to the provider:

    curl http://127.0.0.1:8787/v1/models                     # every route
    curl "http://127.0.0.1:8787/v1/models?provider=xkiro"    # one provider's routes

One upstream call per provider per request, made with an eligible account's key;
a model list spends no tokens and gives the account's reservation straight back. A
provider whose catalog cannot be read does
not break the list: its routes are still returned, without that provider's
fields, and the reason is logged. A route that the provider's catalog does not
mention is still listed -- the route is what works, and its absence from the
catalog is the provider's business -- but with `RUST_LOG=debug` the log prints the
published ids closest to it, which is how a wrong vendor prefix gets found (ours
had `glm/glm-5.3-flash` where xkiro publishes `z-ai/glm-5.3-flash`).

## Credentials

Keys go into the operating system's own credential store (macOS Keychain,
Windows Credential Manager, freedesktop Secret Service) via the keyring crate.
Accounts in the config need no key field at all; omitting it looks the account up
by provider/account name.

    lgw keys add <provider> <account>     # hidden prompt, never an argv flag
    lgw keys add <provider> <account> --stdin
    lgw keys list                         # source and status, never the secret
    lgw keys test <provider> <account>    # prove it by probing the provider
    lgw keys remove <provider> <account>

The key is read from a hidden prompt rather than a command-line argument, because
arguments are visible to every process on the machine and land in shell history.

The gateway has a token of its own as well: the one credential with no provider
behind it, which clients must send once other machines (or other users) can reach
the port. Any string you invent works; it is compared, never forwarded upstream:

    lgw keys add local gateway            # hidden prompt, same as any other key
    # then, under [server] in the config:
    # local_key = { keychain = true }

`local/gateway` is the single reserved name that needs no `[providers.*]` entry.
`lgw keys list` shows it next to the provider keys, `lgw doctor` checks that it
actually resolves (not merely that the config mentions it), and `lgw keys test local
gateway` reports that it is stored -- there is no upstream to probe for this one.

Explicit alternatives are still supported when you want them:
key = { keychain = true }, key = { env = "X" }, key = { key_file = "/run/secrets/x" },
or an inline literal (discouraged).

### macOS: why it keeps asking for your password

macOS binds a keychain item's access control to the **code signature** of the
program asking for it. `cargo install` produces an *ad-hoc* signature, whose
identity is the hash of the code itself, so every rebuild looks like a different
application to the keychain and an "Always Allow" grant does not survive it.

On top of that, a grant is per ITEM, and an older build kept one keychain item
per account: a cold start asked once per account, and `status`, `accounts probe`
and `doctor` asked again, because each is a new process reading the same items.
A test on this checkout read every credential in 24 ms once that was fixed;
before, it waited on three prompts.

Two things fix it:

    scripts/lgw-identity.sh       # once: creates and trusts the certificate
    scripts/lgw-install.sh        # cargo install, then signs the result

The first makes the grant stick, because the access-control entry then names a
certificate that does not change when the code does; the second signs the binary
that actually gets installed. Both are optional -- without them the gateway
behaves identically, it just keeps asking.

There is also only one item to grant: every key lives in a single `vault` entry,
which is where `lgw keys add` has always written them, and `lgw keys list` shows
which item each key came out of. Nothing needs migrating, and there is no command
to remember.

If you are upgrading from a build that predates the vault, its per-account items
are no longer read: `lgw keys add <provider> <account>` stores the key in the
vault, and the old item can be deleted in Keychain Access.

Note that a plain `cargo install --path .` replaces the signed binary with an
ad-hoc one and brings the prompts back; use `scripts/lgw-install.sh` instead.
Re-signing restores the existing grant, so there is nothing to re-authorise.

If you would rather not use the credential store at all:

    [providers.xkiro.accounts.acc-1]
    key = { key_file = "~/.light-ai-gateway/keys/xkiro-acc-1" }   # chmod 600

A 0600 file keeps other users and accidental commits out. It does not keep a
backup or a disk image out, which is the one thing the credential store adds.

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

Set LGW_CREDENTIAL_BACKEND=memory to force an ephemeral store, which is useful
for tests and dry runs and never touches your real keychain.

## Commands

    lgw serve                          start the proxy
    lgw status [--refresh]             account board
    lgw usage --since 7d --group-by provider,model,account
    lgw prices [--drift-only]          configured price vs the price learned from real spend
    lgw keys add|list|remove|test      provider credentials
    lgw accounts list|probe|disable    account maintenance
    lgw accounts enable <id>           put a disabled account back
    lgw tail                           recent requests
    lgw doctor                         config, credentials, probe reachability
    lgw init                           write an example config

## Reading the output

Every table measures its own columns, so a model name that grows does not push
the rest of the row out of line, and the numbers are printed at the scale they
are read at: token counts as `921.4K` and `1.0M`, money with one decimal for a
total and enough decimals for a per-request cost that a one-decimal rule would
round away.

Headings -- a table's header row, a section title, the model a price panel belongs
to -- are bold cyan. Cyan is the one colour with no meaning attached to it, so a
heading cannot be misread as a state: green is working, yellow is weakening, red
is wrong. The dimmed lines are legends and fix-up advice, not readings.

Beyond that, colour marks the few readings that need attention -- a state that is
not ACTIVE, a reading that is STALE or only estimated, a price that has drifted, a
request that spent metered money -- and nothing else. It never carries information
on its own: the words say the same thing with the colours off. Colour is off
whenever the output is not a terminal, so a pipe, a redirect and a log file get
exactly the bytes they got before:

    lgw --color always status     # colour a capture or an editor's run panel
    lgw --color never status      # never colour, whatever the terminal says

`NO_COLOR=1` turns it off and `CLICOLOR_FORCE=1` turns it on, which is how a
terminal that cannot be detected (`docker run -t`, a CI that captures the stream)
keeps its colours.

## Account recovery

Two different things take an account out of the pool, and they need different
answers.

**A failure disables it** -- a 401, 403 or 402. The pool stops using it until a
probe shows it is both reachable and funded:

    lgw keys add <provider> <account>    # first, if the credential was the problem
    lgw accounts probe <id>

A probe that comes back with headroom puts the account back on its own. One that
succeeds but still shows nothing left does not, deliberately: an account that is
out of money would otherwise re-enter rotation once per poll and fail every time.

**An operator disables it** -- `lgw accounts disable <id>`. That is a decision,
and a probe will not undo it: a probe proves the credential works, not that you
changed your mind. The way back is explicit:

    lgw accounts enable <id>

That sets the account to UNKNOWN rather than ACTIVE, so a probe has to confirm it
before it takes traffic -- the same rule a restart follows.

Both are per account: `accounts disable X` writes X's row alone and leaves every
other account's quota readings and reset times untouched.

## Prices

The gateway must know what a request costs before it sends it: that estimate is
what the hard gate compares against the remaining allowance. Two numbers feed it.

**Configured** is what you say a model costs, per provider, named the way a
pricing page names it and in USD per million tokens:

    [prices.xkiro."z-ai/glm-5.3-flash"]
    input = 0.30
    output = 1.20
    "cache read" = 0.03     # optional; defaults to input / 10

`cache read` is a price, not a ratio, so it reads the same as the other two.
Leaving it out assumes a tenth of input rather than zero: a cache read that looks
free would under-estimate cost, and under-estimating is the direction that spends.

**Learned** is what the spend difference between two quota probes says it
actually costs. Providers apply per-model multipliers, promotions and credit
rates, so observation is the more trustworthy figure, and it takes over for
billing as soon as there is one. The configured value is kept unchanged.

Two conditions have to hold before that difference means anything, and both fail
easily in practice:

- **The account must be used only through this gateway.** The provider reports
  spend per account, not per client. Use the same account from an editor and its
  spend lands in the interval and gets charged to the gateway's own tokens.
- **The interval must carry a real token volume.** A spend figure divided by a
  handful of tokens is a fixed per-request charge wearing a rate's clothes. One
  live Command Code interval turned 47 tokens and 0.073 credits into 1559 USD
  per million, which then refused every request over ~5,400 tokens. Rates above
  1000 USD per million are now rejected rather than stored, and so is any such
  row read back from an older ledger.

When either condition is doubtful, write the price down instead of waiting for it
to be learned. A configured price is never overwritten by a bad interval, and one
plausible number is worth more than a table that quietly wedges the hard gate.

    lgw prices

    PROVIDER  MODEL               SOURCE   MIX       EXPECTED  ACTUAL  DRIFT      SAMPLES
    xkiro     z-ai/glm-5.3-flash  learned  79/1/20%  $0.2554   $0.9    +252.3% !       12

Both money columns are **USD per million tokens over the same observed mix**:
EXPECTED is your configured prices applied to that mix, ACTUAL is what the
observed spend works out to. Read together they say "I expected $0.26 per
million and I am paying $0.9".

The MIX column is not decoration. The same prices cost very different amounts
per million on different mixes -- measured, $0.2166/M on an input-heavy mix
against $0.9/M on an output-heavy one -- so a per-million figure only means
something next to the mix it was measured on. Never compare one across mixes.

DRIFT is the exception: it is zero whenever prices match, whatever the mix,
which is why it is the thing to watch and the money columns are the explanation.

A learned rate is a single blended figure. One probe gives one equation for
three unknown prices, so input and output cannot be separated unless your usage
mix varies; the split is therefore trusted only where you configured it.

That combination is a monitoring tool, not just a display. If the two agree,
nothing has changed. If they diverge by 20% or more the row is flagged and a
warning is logged, which means the upstream price is no longer what you were
told -- a repricing you heard about, or one you did not. `--drift-only` prints
just those rows.

A model with no configured price and no observation yet is priced at a
deliberately conservative default, so an unpriced model can never look free.

Learned prices are persisted, so a restart does not fall back to that default.

## The two pots of money

Every quota check is one of two kinds, and the gateway never conflates them:

| Class | Examples | Behaviour |
|---|---|---|
| **Covered** | xKiro 5h/7d spend windows, Command Code monthly credits | Prepaid by a subscription. Forfeited when the window resets. Preferred. |
| **Metered** | xKiro wallet, Command Code purchased credits, DeepSeek balance | Real money. Permanent once spent. Refused unless the operator opts in. |

A third kind sits outside this table: xKiro's daily **free-token allowance**. It
is read and stored, but it is counted in tokens rather than money, so it takes no
part in routing, in the hard gate or in the board. Exhausting it blocks free
models only, and free models are not routed here; letting it retire an account
would take paid models offline because an allowance nobody was spending ran out.
It reaches the client as the provider's own 429.

The distinction is not cosmetic. Command Code does not return an error when a
5-hour window is exhausted; it silently bills your purchased credits instead, so
the request succeeds while your balance drops. A gateway that only reacts to 429s
will spend money without ever telling you. The hard gate below is the guard.

## How an account is chosen

For each request:

1. **Hard gate.** Only accounts whose covered headroom covers the estimated cost
   plus a reserve are candidates. The reserve is the larger of the p95 of recent
   request costs and what that account already has committed in flight, plus a
   configured margin. That is what stops a burst from overflowing into paid
   credits: twenty concurrent requests cannot each size the gate as though the
   other nineteen were not there.
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
| 529 / 5xx | upstream trouble | bounded retry on a **different** account |
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

## When it does not work

Start with `lgw doctor`. It parses the config, resolves every credential, probes
every provider, checks the two things a model name can silently disagree about --
a price entry no route can reach, and a route the provider's own `/v1/models` does
not publish (it names the closest id it does publish) -- flags a port other
machines can reach, and exits non-zero with a count of what is wrong.

The second check is worth explaining: a route key is the model id sent upstream,
so a wrong vendor prefix (`glm/glm-5.3-flash` where the provider has
`z-ai/glm-5.3-flash`) is a request the provider refuses -- and it also orphans
whatever price entry was written for the real id, leaving the route gated at the
conservative default of 15 in / 75 out per million. Both halves of that are
reported, separately, because either one can happen alone.

| Symptom | Where to look |
|---|---|
| `no eligible account` on every request | `lgw status` -- every account may be parked, disabled, or under the hard gate. The 429 body names the account with the most room, how much it has left, and the estimate that did not fit |
| An account stays DISABLED after you fixed the key | a probe only restores an account that also shows headroom, and an operator disable is never undone by one -- see [Account recovery](#account-recovery) |
| A password prompt on every run (macOS) | the grant is bound to the code signature, and an ad-hoc one changes on every rebuild: run `scripts/lgw-install.sh` |
| A price looks wrong | `lgw prices --drift-only`. A learned rate is an observation, not the truth, and a configured price is never overwritten by one |
| Every request is 429 with a Retry-After of weeks | a covered window is spent and the earliest reset is that far away. `lgw status` shows a countdown per gate |
| The board is empty after moving the database | the ledger path is `server.db_path` in the config. An empty board means "not probed yet", never "no accounts" |

## Non-goals

Cross-provider fallback, cross-model downgrade, protocol translation, semantic
caching, multi-user RBAC and a web UI are all deliberately out of scope. A model
maps to exactly one provider; changing that is a product decision, not something
the gateway should infer.

## Development

    cargo test                              # the whole suite, a couple of seconds
    cargo clippy --all-targets -- -D warnings
    cargo fmt --check

There is no CI workflow: those three plus a release build are what to run before
a change leaves this machine. There is no tests/ directory either -- the suite
lives beside the code it covers, in each module, and the tests are named as the
behaviour they assert.

Provider quota adapters live in src/quota. Adding a provider means implementing
one trait and declaring its base URL, credentials and error vocabulary.
`adapter = "none"` is deliberately not supported: an account is admitted to the
pool only after a successful probe, so a provider with no quota source can never
be selected. Use xkiro, command-code or deepseek.

## License

MIT. See [LICENSE](LICENSE). Use it, modify it, ship it, sell it -- it comes with
no warranty, and it spends real money when you configure it to.

## Safety notes

- Binds to loopback by default and holds paid credentials; set a local key if
  anything else can reach the port. `lgw doctor` reports both a missing
  `local_key` and a non-loopback listen address as problems.
- Keys come from the environment or a dedicated file, are never logged, and are
  redacted in every debug or display path.
- Probe hosts are pinned in configuration, and redirects are refused outright, so
  a credential cannot be sent to an address supplied with a request or named in a
  Location header.
- A failed quota probe keeps the previous reading and marks it stale. It never
  resets a reading to zero, which would both overstate availability and let the
  CLI claim a funded account is empty.
- Prompt and response bodies are never stored.
