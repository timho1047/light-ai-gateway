# Routing and the two kinds of quota

**English** | [简体中文](routing.zh-CN.md)

Why the gateway prefers covered allowance, how it chooses an account, and what
happens when the pool is empty.

## The two pots of money

Every quota check is one of two kinds, and the gateway never conflates them:

| Class | Examples | Behaviour |
|---|---|---|
| **Covered** | xKiro 5h/7d spend windows, Command Code monthly credits | Prepaid by a subscription. Forfeited when the window resets. Preferred. |
| **Metered** | xKiro wallet, Command Code purchased credits, DeepSeek balance, HaoAI balance | Real money. Permanent once spent. Refused unless the operator opts in. |

A third kind sits outside this table: xKiro's daily **free-token allowance**. It is
read and stored, but counted in tokens rather than money, so it plays no part in
routing, the hard gate or the board. It reaches the client as the provider's own
429.

The distinction matters. Command Code does not return an error when a 5-hour window
is exhausted; it silently bills your purchased credits, so the request succeeds
while your balance drops. That is what the hard gate guards against.

The opt-in is answered per provider. `[policy] allow_metered` decides for
everyone, and `[providers.<name>] allow_metered` overrides it for one provider,
in either direction -- which is what lets a wallet-only provider such as DeepSeek
or HaoAI serve without also unlocking Command Code's overflow.

## How an account is chosen

For each request:

1. **Hard gate.** Only accounts whose covered headroom covers the estimated cost
   plus a reserve are candidates. The reserve is the larger of the p95 of recent
   request costs and what that account already has committed in flight, plus a
   configured margin.
2. **Sticky first.** If the session is already bound to a candidate, keep it.
   Moving a conversation discards the provider's prompt cache and re-bills the
   whole history at full input price. The session key is the first non-empty of
   `x-session-id`, `x-session-key`, `x-conversation-id`, or the `user` field
   of the JSON body.
3. **Consume-first.** Among candidates below the soft threshold, take the most
   used. Draining one account before moving on minimises switches, and every
   switch costs a cache miss.
4. **Soft threshold is a preference, not a wall.** If every account is above 80%
   the least-used one is still used. A threshold you configured must not become
   an outage while headroom genuinely remains.
5. **All accounts unavailable.** HTTP 429 with a real Retry-After, taken from the
   earliest window reset. It does not silently fall through to another provider,
   and it does not silently spend metered balance.
