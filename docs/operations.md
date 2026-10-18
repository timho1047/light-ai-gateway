# Operations and troubleshooting

**English** | [简体中文](operations.zh-CN.md)

Running the gateway day to day: reading the board, recovering accounts,
understanding failures, and the metrics that explain a switch.

## Reading the output

Every table measures its own columns, so a longer name does not push the rest of the
row out of line, and numbers are printed at the scale they are read at: token
counts as `921.4K`, money with one decimal for a total and enough decimals for a
per-request cost.

Headings are bold cyan. Cyan carries no meaning, so a heading cannot be misread as a
state: green is working, yellow is weakening, red is wrong. Dimmed lines are
legends and advice, not readings.

Colour marks only the readings that need attention -- a state that is not ACTIVE, a
reading that is STALE or only estimated, a price that has drifted, a request that
spent metered money -- and nothing else. It carries no information of its own, and
it is off when the output is not a terminal, so pipes, redirects and logs get the
bytes they always did:

    lgw --color always status     # colour a capture or an editor's run panel
    lgw --color never status      # never colour, whatever the terminal says

`NO_COLOR=1` turns it off and `CLICOLOR_FORCE=1` turns it on, for terminals that
cannot be detected (`docker run -t`, a CI that captures the stream).

## Account recovery

Two different things take an account out of the pool, and they need different
answers.

**A failure disables it** -- a 401, 403 or 402. The pool stops using it until a
probe shows it is both reachable and funded:

    lgw keys add <provider> <account>    # first, if the credential was the problem
    lgw accounts probe <id>

A probe with headroom puts the account back. One that succeeds but still shows
nothing left does not: it would otherwise re-enter rotation every poll and fail
every time.

**An operator disables it** -- `lgw accounts disable <id>`. That is a decision,
and a probe will not undo it: a probe proves the credential works, not that you
changed your mind. The way back is explicit:

    lgw accounts enable <id>

That sets the account to UNKNOWN rather than ACTIVE, so a probe has to confirm it
before it takes traffic -- the same rule a restart follows.

Both are per account: `accounts disable X` writes X's row alone and leaves every
other account's quota readings and reset times untouched.

## Failure handling

A 429 means several different things and each needs a different response:

| Signal | Meaning | Action |
|---|---|---|
| 429 with a short Retry-After | pacing | retry, preferring an account this request has not used; the same one only when nothing else is eligible |
| 429 with a long Retry-After, or a quota-shaped code | window exhausted | park the account, move on |
| 402 / insufficient_quota | out of money | disable the account, alert, and never send it another request; this request is replanned elsewhere if an account is eligible |
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
not publish (naming the closest id it does publish) -- flags a port other machines
can reach, and exits non-zero with a count of what is wrong.

A route key is the model id sent upstream, so a wrong vendor prefix is both a
request the provider refuses and an orphaned price entry that leaves the route
gated at the conservative default of 15 in / 75 out per million.

| Symptom | Where to look |
|---|---|
| `no eligible account` on every request | `lgw status` -- every account may be parked, disabled, or under the hard gate. The 429 body names the account with the most room, how much it has left, and the estimate that did not fit |
| An account stays DISABLED after you fixed the key | a probe only restores an account that also shows headroom, and an operator disable is never undone by one -- see [Account recovery](#account-recovery) |
| A password prompt on every run (macOS) | the grant is bound to the code signature, and an ad-hoc one changes on every rebuild: run `scripts/lgw-install.sh` |
| A price looks wrong | `lgw prices --drift-only`. A learned rate is an observation, not the truth, and a configured price is never overwritten by one |
| A learned price reads far above the card | the account was used outside the gateway, so that spend was charged to these tokens: `lgw prices --forget <provider> <model>`, then restart `lgw serve` |
| Every request is 429 with a Retry-After of weeks | a covered window is spent and the earliest reset is that far away. `lgw status` shows a countdown per gate |
| The board is empty after moving the database | the ledger path is `server.db_path` in the config. An empty board means "not probed yet", never "no accounts" |
