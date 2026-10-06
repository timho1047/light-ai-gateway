# light-ai-gateway

An OpenAI-compatible gateway that holds several provider accounts and routes each
request to an account whose prepaid allowance can pay for it. Because some
providers bill silently when an allowance runs out, the distinction between money
already paid for and money about to be spent is the centre of this model.

## Language

### Money and quota

**Account**:
One credential for one provider, plus the last-known state of that provider's
quota for it.
_Avoid_: key, seat, user, login

**Account id**:
The canonical identity of an Account, which is its provider and its id taken
together. Unique only within a provider.
_Avoid_: id

**Provider**:
The upstream service an Account belongs to.
_Avoid_: vendor, backend

**Gate**:
One limit that a provider enforces on one Account, with the cap it is counted
against and how much of it is consumed.
_Avoid_: limit, quota (quota is what the gates together describe)

**GateKind**:
Which concrete limit a Gate is: 5h, 7d, 1mo, free-token allowance, wallet,
purchased credits, or requests-per-minute.

**GateClass**:
Which pot of money a Gate meters. Every routing and spend decision turns on it.
_Avoid_: type, category

**Covered**:
Quota already paid for by a subscription. Forfeited when its window resets, so
spending it costs nothing new.
_Avoid_: free tier, prepaid, plan quota

**Metered**:
Quota bought with real money or credits. Spending it is permanent.
_Avoid_: paid, wallet (a wallet is where metered quota lives, not the class)

**Free-token allowance**:
A daily token allowance that is not money at all. It gates free models only: it
never pays for a request, never retires an Account, and never reaches the Board.
_Avoid_: free quota, free tier

**Pacing limit**:
A requests-per-minute limit. Not money; it only delays.
_Avoid_: rate limit (that is also the HTTP status this produces)

**Reading**:
The cap, consumption and reset time of a Gate as last observed, with the time it
was observed and how much it is trusted.
_Avoid_: sample, measurement, value

**Stale**:
A Reading carried over from a probe that failed. It is never rendered as zero.
_Avoid_: expired, old

**Window**:
The rolling period a Covered Gate resets on. Windows nest: a weekly window
contains the 5-hour windows inside it.
_Avoid_: period, interval

**Interval**:
The stretch between two probes of one provider. The tokens served inside it are
what the spend it moved gets attributed to.
_Avoid_: tick, period, cycle

**Drift**:
A provider Reading that disagrees with the Ledger by more than the configured
threshold. The provider always wins.
_Avoid_: mismatch, discrepancy

### The pool

**Pool**:
The set of Accounts, and the only place that decides which of them may serve a
request.
_Avoid_: registry, roster, manager

**Hard gate**:
The check that refuses a request whose estimated cost does not fit inside an
Account's Covered headroom plus a reserve. It is what stands between a spent
window and real money.
_Avoid_: budget check, limit check

**Soft threshold**:
The utilisation above which an Account takes no new sessions but keeps serving
the ones already bound to it.

**Consume-first**:
The rule that the most-used eligible Account is chosen, so switching (and the
lost prompt cache that comes with it) stays rare.

**Session key**:
The client-supplied identifier that binds a conversation to one Account.
_Avoid_: conversation id, user id

**Sticky**:
Bound to a Session key. A sticky session outlives the soft threshold.

**In-flight commitment**:
The estimated cost of an Attempt that has been accepted but not yet settled.
Counted against the Account's headroom until it is.
_Avoid_: hold, booking

**Reservation**:
The one value that owns an In-flight commitment. Settling it spends the attempt;
dropping it unsettled gives the commitment back. An Attempt has exactly one, and
nothing else can return it.
_Avoid_: lease, claim, hold

**Account state**:
Where an Account sits in the pool: Active (serving), Draining (sticky only),
Parked (a gate is spent; returns at a known time), Disabled (needs an operator),
Unknown (restored from disk, not yet probed, must not serve).

### Serving a request

**Route**:
The single mapping from a client's model name to the provider and model name
sent upstream.
_Avoid_: alias, mapping

**Gateway**:
The process that owns the Pool, the prices and the Ledger, and serves requests.

**Attempt**:
One try against one Account. A request may take several, and each is recorded on
its own so retries do not pollute the timings.
_Avoid_: try, retry

**Coverage**:
Which pot paid for a request: Covered or Metered.
_Avoid_: billing mode

**Upstream**:
The provider's HTTP API, as the gateway sees it.
_Avoid_: backend, origin

**Failure class**:
What an upstream failure means for the next move: pacing, window exhausted, out
of money, bad credentials, upstream trouble, or a bad request. It decides
whether the request retries, and on which Account.

### Probing

**Probe**:
One provider's way of reading its own quota into Gates.
_Avoid_: adapter, client, poller

**Metered opt-in**:
The operator's permission to spend real money, granted for every provider at
once or for one provider on its own.
_Avoid_: allow_metered flag

### Pricing

**Price**:
What a model costs per million tokens, as configured by the operator.

**Learned price**:
A rate derived from spend observed between probes. An observation, never the
truth: a configured Price is not overwritten by one.

**Tier**:
One price for one time window, such as peak or off-peak. The default price
applies whenever no window matches.

**Schedule**:
The set of Tiers for one model.

**Window name**:
The identity of a Tier. It is the key a learned rate is stored under, so two
Tiers may not share one and it may not be empty; the default price is the absence
of a name.
_Avoid_: tier index, tier number, position

**Credit multiplier**:
The factor between an Account's own unit and the currency a Price is written in.

**Fallback price**:
What an unpriced model is charged at. Deliberately over-stated, so an unknown
model can never quietly drain a wallet.

### The ledger and its reports

**Ledger**:
The persisted record of Accounts, Gates, requests and learned prices, and the
only thing a restart trusts.
_Avoid_: database, store, state file

**Board**:
The table of Accounts and their Readings, Covered and Metered, as the operator
reads it.

**Usage report**:
The offline document describing the pool's quota and history, served for a
dashboard.
