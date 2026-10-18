# Prices

**English** | [简体中文](prices.zh-CN.md)

How the gateway estimates what a request costs: configured prices, learned
prices, drift, and how to forget a learned rate.

The gateway must know what a request costs before it sends it: that estimate is
what the hard gate compares against the remaining allowance. Two numbers feed it.

**Configured** is what you say a model costs, per provider, named the way a
pricing page names it and in USD per million tokens:

    [prices.xkiro."z-ai/glm-5.3-flash"]
    input = 0.30
    output = 1.20
    "cache read" = 0.03     # optional; defaults to input / 10

`cache read` is a price, not a ratio, so it reads like the other two. Leaving it
out assumes a tenth of input rather than zero.

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
  handful of tokens is a fixed per-request charge wearing a rate's clothes. Rates
  above 1000 USD per million are rejected rather than stored, and so is any such
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

The same prices cost very different amounts per million on different mixes
($0.2166/M input-heavy against $0.9/M output-heavy), so never compare a
per-million figure across mixes. DRIFT is the exception: it is zero whenever
prices match, whatever the mix.

A learned rate is one blended figure: input and output cannot be separated unless
your usage mix varies, so the split is trusted only where you configured it.

If the two agree, nothing has changed. If they diverge by 20% or more the row is
flagged. A warning is logged from a lower line, `policy.drift_threshold`
(default 0.05), so a repricing is named in the log before the board marks it.
`--drift-only` prints just the marked rows.

A model with no configured price and no observation yet is priced at a
deliberately conservative default, so an unpriced model can never look free.

Learned prices are persisted, so a restart does not fall back to that default.

A learned rate is the mean over the whole history, so a row polluted by outside
usage stays too high forever. Forget it and the configured price is in effect
again:

    lgw prices --forget                          # every learned row
    lgw prices --forget xkiro                    # one provider
    lgw prices --forget xkiro z-ai/glm-5.3-flash # one model

    PROVIDER  MODEL               TIER  RATE     CACHE READ  SAMPLES
    xkiro     z-ai/glm-5.3-flash     0  $0.4071  $0.0081          44

A running `lgw serve` keeps its own copy of the table and writes it back on the next
probe, so restart it for the change to stick. Forgetting a model that has no
configured price drops it to the conservative default -- write the price down
first, or leave the row alone.

Spend that moves between probes while the gateway has no tokens to charge it to is
logged as unattributable and not learned: the account is being used somewhere else,
which is exactly when a learned rate stops meaning anything.
