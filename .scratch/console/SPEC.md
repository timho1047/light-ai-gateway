# Browser console for light-ai-gateway

**English** | [简体中文](SPEC.zh-CN.md)

**Status:** spec for implementation. Related decisions: ADR-0001 (console is a
loopback-only operator surface), ADR-0002 (config file is the source of truth),
ADR-0003 (apply is atomic, running requests keep their snapshot), ADR-0004
(console ships inside the binary). Contract: docs/web-ui.md. Domain language:
GLOSSARY.md.

## Problem Statement

Configuring the gateway means editing a TOML file and running CLI commands. The
file is the slowest part of an operator's week: a typo is only discovered when
the next command fails, a change cannot be applied without restarting the
process, and the restart drops every request that is streaming at that moment.
Seeing what the pool is doing means running several commands and reading
terminal tables; comparing spend across a week or finding a request that went
wrong means reconstructing the query by hand. The operator has no single place
that shows the pool, answers usage questions, and accepts a configuration
change with the same validation the gateway itself uses.

## Solution

`lgw serve` also serves a browser console on the same listener, for the same
machine. It shows the Board, usage and spend, recent requests, prices and
drift, the doctor's findings, and the credential sources; it offers the actions
the CLI has; and it edits `config.toml` in a form. A saved change is written
back to the same file and applied to the running process without dropping
in-flight requests: a request keeps the runtime snapshot it started with, a new
request sees the new one, and only settings that would rewire the process wait
for a restart (and say so). The console has no login, because it answers
loopback peers only and is guarded against a page in another browser tab.

The console is one operator's tool, not a client API. The OpenAI-compatible
`/v1` surface, its authentication and its JSON contracts do not change. The
copy switches between English and Simplified Chinese, the default is light with
a night mode, and everything ships inside the binary with no Node build step.

## User Stories

### Access and safety

1. As an operator, I want the console at the gateway's own address after
   `lgw serve`, so that I do not need a second process or port.
2. As an operator, I want the console to answer loopback peers only, so that
   exposing the proxy listener to a network does not expose configuration
   editing.
3. As an operator, I want console pages and actions to work without a login,
   cookie or session, so that there is no second credential to manage.
4. As an operator, I want a page in another browser tab to be unable to change
   the configuration, so that visiting an untrusted page cannot spend my quota.
5. As an operator, I want the console to refuse a request whose `Host` is not
   the loopback listener, so that DNS rebinding cannot reach it.
6. As an operator, I want a non-loopback listener to keep the console reachable
   through `127.0.0.1`, so that my normal local workflow still works.
7. As an operator, I want every console response to carry a restrictive
   content security policy, no framing and no-store caching, so a console page
   cannot be embedded or cached by another site.
8. As an operator, I want the console to leave `/v1` untouched, including its
   `local_key` bearer rules, so existing clients and widgets do not notice the
   console at all.
9. As an operator, I want the console never to display, return or log a provider
   key or `local_key` value, so that a screen share or a log file cannot leak a
   credential.
10. As an operator, I want a non-loopback peer to receive the same 404 for
    every console path, so that the surface does not advertise itself.

### The Board

11. As an operator, I want one page listing every Account with its provider,
    label and state, so that I can see the pool at a glance.
12. As an operator, I want each Account's Covered windows with used and cap per
    window, so that I can tell a spent 5-hour window from a spent monthly one.
13. As an operator, I want each Account's Metered balance or wallet, so that I
    know what real money remains.
14. As an operator, I want reset countdowns per window, so that I know when an
    Account comes back without computing timestamps in my head.
15. As an operator, I want Covered allowance and Metered money visually and
    textually distinct and never merged, so that consuming a subscription is
    never mistaken for spending cash.
16. As an operator, I want a stale Reading flagged as stale with its age,
    never rendered as zero, so that a failed probe does not look like a fresh
    empty quota.
17. As an operator, I want a Disabled, Draining or Parked Account to show its
    state, last error and return time, so that I know whether to probe, re-key or
    wait.
18. As an operator, I want inventory percentages for each window across the
    pool, so that I can judge the pool as a whole beside each Account.
19. As an operator, I want at-a-glance vitals (requests per minute, in-flight,
    p95 TTFT, recent errors, cache hit rate), so that I can tell healthy from
    struggling without leaving the page.
20. As an operator, I want the Board to refresh on a timer without probing
    providers, so that watching it costs no quota.
21. As an operator, I want a Refresh button and a Probe all action, so that I
    can force a new Reading when I suspect the last one is old.
22. As an operator, I want a price-drift summary on the Board, so that a quiet
    repricing is visible where I look first.
23. As an operator, I want the Board to render the last known state even when
    no Account has been probed yet, with "not probed yet" instead of a fake
    empty table, so that a fresh ledger is not mistaken for an empty pool.

### Usage and history

24. As an operator, I want spend over time split into Covered and Metered, so
    that I can see whether a change made the gateway burn subscriptions or cash.
25. As an operator, I want token counts (input, output, cached) and cache hit
    rate, so that I can watch the cost metric account switching destroys.
26. As an operator, I want request counts and failure counts for the same
    range, so that spend is read beside traffic.
27. As an operator, I want to group usage by provider, model, account or day,
    so that I can answer "which model costs the most" without a SQL terminal.
28. As an operator, I want ranges of 24 hours, 7, 30 and 90 days and all, so
    that the usual comparison windows are one click.
29. As an operator, I want money totals with the ledger's exact decimal
    precision and per-currency separation, so that the dashboard agrees with the
    ledger and never sums different currencies.
30. As an operator, I want TTFT percentiles and TPS over time, so that a
    provider slowing down is visible before it becomes a complaint.
31. As an operator, I want the console to say that latency history stops at 30
    days because attempt detail is pruned, so that a missing earlier point is
    expected rather than a bug.
32. As an operator, I want a recent-requests table with attempt-level status,
    coverage, tokens, TTFT, TPS and cost, so that I can find the request that
    failed or spent money.
33. As an operator, I want to filter the request list by errors and by
    coverage, so that I can isolate what went wrong.
34. As an operator, I want a prices page showing configured vs learned prices
    per tier and the drift between them, so that I can decide whether to
    re-price or forget a learned rate.
35. As an operator, I want to forget a learned price (or all of them) with a
    confirmation, so that a polluted observation stops affecting billing.

### Configuration editing

36. As an operator, I want the console to show which configuration file is in
    use and when it was last applied, so that I never edit the wrong file.
37. As an operator, I want to edit policy, providers, accounts, routes and
    prices in a form, so that I do not have to remember TOML syntax.
38. As an operator, I want to add and remove providers, accounts, routes and
    priced models from the form, so that routine changes stay in one place.
39. As an operator, I want the same validation the CLI uses, with the
    first error naming the field, so that the console cannot bless a file the
    next command would reject.
40. As an operator, I want a TOML diff of my draft before I apply it, so that I
    see exactly what will change.
41. As an operator, I want an invalid draft to change nothing, so that a
    mistake cannot take the gateway down or leave half a configuration behind.
42. As an operator, I want comments, key order and fields the form does not
    know to survive a save, so that the file stays hand-maintainable.
43. As an operator, I want to be told when the file changed under my form and
    asked to reload instead of having my edit overwritten, so that my editor and
    the console can both be open.
44. As an operator, I want each field marked as applying live or needing a
    restart, so that I know what an apply will and will not do.
45. As an operator, I want restart-only settings to be explained in the form and
    refused by an apply by name, so that a setting that cannot take effect is
    never silently ignored.
46. As an operator, I want a read-only raw TOML view for advanced values, with
    literal secrets redacted, so that I can inspect the file without leaking a
    key to the screen.
47. As an operator, I want credential fields to show their source
    (credential store, environment, file, literal present) and never their
    value, so that I can audit my setup safely.
48. As an operator, I want to add or rotate a provider key from the console,
    written only to the OS credential store, so that onboarding a new account
    does not require a terminal.
49. As an operator, I want the console to show what an apply did: what is live,
    what needs a restart, and any error, so that I am never guessing.
50. As an operator, I want a configuration change made in my editor to be
    reloaded by the running process with the same validation, so that the file
    is the only interface and there is no second path.
51. As an operator, I want an invalid external edit to keep the running
    configuration and be reported, so that a bad save in my editor cannot take
    the gateway down.

### Applying a change

52. As an operator, I want an apply to leave requests already in flight
    untouched, so that a configuration change never truncates a stream or
    changes its price mid-request.
53. As an operator, I want a request's retries to use the same snapshot that
    admitted it, so that one request cannot be priced and routed under two
    configurations.
54. As an operator, I want requests that start after an apply to use the new
    snapshot immediately, so that a change takes effect without a restart.
55. As an operator, I want the pool's live state (gates, sticky bindings,
    in-flight commitments, pacing windows) to survive an apply, so that reloading
    does not lose spend or reassign sessions.
56. As an operator, I want a removed Account to stop taking new work but let its
    in-flight attempts settle, so that removing an Account does not fail a request
    that is already running.
57. As an operator, I want a new Account to enter the pool as unknown and be
    probed before serving, so that a stale ledger never re-spends quota.
58. As an operator, I want changing a provider's adapter or base URL to reset
    that provider's Accounts to unknown and re-probe them, so that a Reading from
    the old endpoint is never trusted.
59. As an operator, I want two applies to be serialised, so that a file change
    and a console save cannot interleave into a configuration neither asked for.
60. As an operator, I want the file watcher to ignore the console's own write,
    so that one save does not cause a second redundant apply.
61. As an operator, I want provider probing after an apply to happen in the
    background, so that saving a configuration is instant even when a provider
    is slow.
62. As an operator, I want a failed apply to leave the old snapshot serving and
    report the failure, so that a reload is never worse than no reload.

### Actions and CLI parity

63. As an operator, I want every CLI action available in the console, so that I
    do not have to switch tools for routine maintenance.
64. As an operator, I want to run doctor from the console and see its checks and
    failures, so that setup problems are diagnosable in one place.
65. As an operator, I want to probe one Account or all of them, so that I can
    refresh a suspect Reading on demand.
66. As an operator, I want to disable an Account with a confirmation and to
    enable it again, so that an operator decision is explicit and reversible.
67. As an operator, I want the console to explain that a probe cannot undo an
    operator disable, so that the recovery rules from the CLI hold here too.
68. As an operator, I want to list credential sources, test one, and remove one,
    so that key maintenance does not need a terminal.
69. As an operator, I want a recent-request limit control, so that the tail is
    short enough to read.
70. As an operator, I want long actions to show that they are running and to
    fail with a clear message on timeout, so that a slow provider does not look
    like a frozen page.
71. As an operator, I want `lgw init` not to be offered, so that the console
    cannot overwrite a configuration that the running gateway is using.

### Language, theme and presentation

72. As an operator, I want the console in English or Simplified Chinese, switched
    from the top bar, so that I can read it in whichever language I think in.
73. As an operator, I want a language link to carry the chosen language across
    pages, so that navigation does not silently reset it.
74. As an operator, I want the language choice remembered in this browser
    without a cookie, so that my next visit starts in it.
75. As an operator, I want a light theme by default and a night theme switch, so
    that the console is comfortable at any hour.
76. As an operator, I want the theme choice remembered in this browser, so that
    it does not flash back on every page.
77. As an operator, I want colour to mark only what needs attention and never to
    carry meaning on its own, so that a colour-blind operator reads the same
    facts.
78. As an operator, I want Covered and Metered to keep the CLI's visual
    language and number formatting, so that the terminal and the browser agree.
79. As an operator, I want tabular figures and aligned money columns, so that
    columns can be compared at a glance.
80. As an operator, I want charts to be readable without hover tooltips and to
    have a text summary, so that the numbers are still reachable.
81. As an operator, I want a layout that works on a laptop and in a narrow
    window, so that the console is usable beside an editor.
82. As an operator, I want keyboard-navigable forms and visible focus states, so
    that the console does not require a mouse.

### Operations and maintainability

83. As a maintainer, I want the CLI and the console to call one operations layer,
    so that a rule cannot drift between the two surfaces.
84. As a maintainer, I want the console tests to drive the same HTTP seam as a
    browser, so that they assert behaviour rather than handler internals.
85. As a maintainer, I want `cargo install --path .` to keep producing one
    self-contained binary with no Node step, so that distribution does not change.
86. As a maintainer, I want the console's copy in two catalogues and its data
    paths shared by both languages, so that a translated page cannot show
    different numbers.
87. As a maintainer, I want an apply to be logged with what changed and never a
    secret, so that an operator can reconstruct a surprising configuration.
88. As a maintainer, I want the console URL logged at startup, so that a new
    operator discovers the surface without reading the README.
89. As a maintainer, I want the console to make no provider call on a read
    endpoint, so that leaving a tab open cannot spend quota.
90. As a maintainer, I want the console to send only same-origin JSON mutations,
    so that no-JavaScript cross-site form can reach a write path.

## Implementation Decisions

### Module shape

- The HTTP layer is separated from the gateway: one module assembles the
  router, authentication middleware and listener; the proxy handlers, the
  frozen `/v1` usage handler and the console routes live beside each other
  rather than in one file.
- The CLI is split into per-command modules with shared configuration-path
  resolution and palette handling. This is a mechanical move done first, with
  `git mv` and the test suite green after each step.
- A new runtime module owns the immutable Runtime snapshot, the apply/reload
  engine and the file watcher.
- A new operations module owns the actions and queries used by both the CLI
  and the console: board data, usage aggregation, tail, prices, doctor, probe,
  account enable/disable, credential list/test/remove. The console has no rules
  of its own.
- A new console module owns the pages, the auto-refresh partials, the action
  fragments, the English/Chinese copy catalogue, the SVG charts and the embedded
  assets registry.
- The existing gateway module keeps the live pool, learned price table, probe
  set, schedules and interval ledger, gaining interior mutability only where the
  snapshot does not own a value.

### The runtime snapshot

- A Runtime snapshot is the immutable, configuration-derived view a request or
  probe uses from admission to settlement: resolved routes and aliases,
  provider configuration, policy values and timeouts, configured price seeds,
  schedules, the probe set, the HTTP client configuration and the resolved
  `local_key` state.
- HTTP handlers capture the snapshot once at admission and pass it down; the
  backend interface stops lending a reference to a gateway-owned configuration.
- Settlement uses the snapshot that admitted the attempt, so estimate, tier and
  cost always come from the same configuration.
- The snapshot is published through a single atomic pointer swap; old snapshots
  stay alive as long as any request holds them.

### Configuration: the file is the truth

- `Config` remains parse-and-validate only; it is not serialised back. Edits
  are applied to the existing TOML document, preserving comments, key order and
  unknown fields, and the result is parsed and validated by the same code every
  command uses.
- A draft is the structured form content for the known sections plus the
  fingerprint of the file it was built from. The server applies the draft onto
  the current document; fields the form does not own are left untouched.
- Validation errors become structured values with a stable code, the field path
  and parameters. The CLI keeps rendering the same English messages; the console
  renders them from the language catalogue.
- The pipeline, in order: parse draft, edit document, parse and validate whole
  config, build the replacement Runtime snapshot (credentials resolved
  leniently, probes prepared), render the diff, write the file atomically as
  0600 through a temporary file and rename, publish the snapshot, reconcile the
  pool, start probes for new or changed Accounts. A failure before the publish
  changes nothing.
- A file fingerprint mismatch answers 409 and asks for a reload; the console
  never overwrites an editor's change.
- Restart-only settings are rendered read-only with their restart named. An
  apply refuses a draft that changes them, with a code the console translates.
- The read-only raw view is generated from the document with every literal
  secret redacted; the raw file bytes are never returned.
- Secrets never round-trip: credential fields show a source; adding or rotating
  a key writes only to the OS credential store; a literal present in the file is
  displayed as redacted and can be replaced only by choosing another source.
- `lgw init` uses the same atomic writer instead of truncating in place.

### Applying a change

- Reconfiguration happens in place. The live pool is never replaced: a
  Reservation holds a clone of it, so a swapped pool would settle real spend into
  a discarded object. The pool gains reconcile and policy-setter operations
  performed under its existing lock.
  - Accounts are diffed by qualified id. Kept Accounts keep gates, readings,
    state, available time, window start, last probe and last error; their
    config-derived fields (base URL, currency, multiplier, limits, metered
    opt-in) are refreshed.
  - Added Accounts enter unknown and are probed asynchronously.
  - Removed Accounts are retired: they accept no new work, stay addressable for
    their in-flight attempts, and are dropped when their last commitment settles.
  - Changing a provider's adapter or base URL resets its Accounts to unknown and
    queues re-probes.
  - Policy values are updated under the pool lock; probe cadence follows the new
    values.
  - The learned price table is preserved; configured prices are re-seeded without
    overwriting learned rates; schedules are replaced; the interval ledger is
    preserved.
  - The probe set and the upstream HTTP client are rebuilt when the relevant
    configuration changes; in-flight attempts keep the client they started with.
- Applies are serialised through one worker. The file watcher and the console
  submit to the same queue; a content fingerprint suppresses the watcher's echo
  of the console's own write.
- The watcher polls the file's modification time, size and content fingerprint
  at a short interval. It requires no new dependency and cannot miss a
  rename-based editor save.
- Restart-only settings: listening address, ledger path, request body limit.
- An apply that fails validation or snapshot construction publishes nothing and
  writes nothing. An apply that fails after the write is reported with the file
  already new; the next start uses it.

### HTTP surface and contracts

- Pages are server-rendered HTML: `/` (Board), `/usage`, `/requests`,
  `/prices`, `/config`, `/keys`, `/doctor`. Assets are served from a fixed
  asset path prefix. Auto-refresh and action results are rendered HTML
  fragments, not a JSON API, so the copy and the numbers have one renderer.
- Mutations accept same-origin JSON only, require `X-LGW-Console: 1` and answer
  with an HTML fragment (or a page-level redirect for a full form submit). The
  handler and the fragment render in the requested language.
- Reads are GETs with no side effects. Mutations are POSTs. Nothing else.
- Console routes are guarded by the loopback peer check, the `Host` allowlist,
  an `Origin` check when present, the custom header, a JSON content type and a
  smaller body limit than the proxy's. They emit no `Access-Control-*` header
  and carry a restrictive CSP, no framing and no caching.
- Non-loopback peers receive 404 for every console path, including assets.
- Status codes used by mutations: 200 applied/valued, 409 conflict or
  restart-only refusal, 422 invalid draft, 503 an action's dependency is
  unavailable (provider unreachable, credential store unreadable), 504 for an
  action timeout. The fragment says which, in the current language.
- Language is selected by `?lang=en|zh` on every page and fragment; links
  carry it. The top-bar switch writes the choice to the browser's local storage
  and navigates with the parameter; a page loaded without the parameter uses the
  stored choice via a one-time replace. No cookie is set.
- Theme is a client-side `data-theme` switch, light by default, persisted in
  local storage and applied by an inline script before first paint.
- Startup logs the console URL at the listening address.
- The console API prefix (`/console/api/*`) promised by `docs/web-ui.md` becomes
  the mutation prefix; responses are HTML fragments, and that document is
  amended to say so.

### Pages and design

- The visual direction is the editorial-ledger prototype: rules instead of
  cards, serif display type for page and section titles, a monospaced tabular
  face for every figure, generous margins, and one accent reserved for metered
  money; the pool table borrows the instrument-panel prototype's density, and
  save/apply controls borrow the product-console prototype's sticky bar.
- Colour is a third channel only: state, staleness, drift and metered spend are
  always also words or shaped marks. Green/yellow/red keep the CLI meanings.
- Copy uses the project's number scales (921.4K, $0.2554), tabular numerals and
  right-aligned money.
- Charts are SVG generated by the server from the same numbers the tables show;
  each chart has a short text summary, so hover is not required.
- Responsive layout for laptop and narrow windows; no phone-first layouts.
- Board auto-refreshes every 10 seconds while visible, Usage every 30; the
  Requests page refreshes on demand. Polling stops while the tab is hidden.
- The console page title, favicon-ish mark and empty states use the project
  vocabulary; "not probed yet" is never rendered as an empty pool.
- Writes require JavaScript for the custom-header JSON request; read pages render
  without it.

### Data and queries

- The Board reads the in-memory pool snapshot; it never probes a provider on
  render.
- Usage, requests and prices read the ledger through a read-only connection; they
  never write.
- Ledger schema version 8 adds indexes only: `requests` by provider, model,
  account and coverage combined with timestamp, and `attempts` by provider and
  start time. No column is rewritten and no money format changes.
- Money is aggregated as Decimal in Rust after an indexed range scan; no float
  sum, and currency totals are kept apart.
- Ranges are 24h, 7d, 30d, 90d and all. Attempt-level detail is limited by the
  existing 30-day pruning; the page says so.
- Actions that reach providers (probe, doctor, key test) run synchronously in the
  request with a bounded timeout; the browser shows a running state.

### Packaging and dependencies

- Assets are embedded in the binary; a development override serves them from
  disk so CSS and copy can be iterated without a rebuild.
- The only new runtime dependency is `toml_edit`. The file watcher is a poll,
  not `notify`; asset serving is a small handler, not `rust-embed` or
  `tower-http`; charts are hand-drawn SVG, not a library.
- Type comes from system font stacks; no font is shipped and nothing is loaded
  from a CDN.
- `cargo install --path . --locked` keeps producing one binary. There is no Node
  step and no CI workflow; the verification commands stay `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` and a
  release build.

### Logging

- An apply logs the file path, whether it came from the console or the watcher,
  the changed sections, any restart-only refusal, the duration, and never
  configuration values that are secret.
- Actions log their target and outcome at info or warn, with the same redaction
  rules as the CLI.

## Testing Decisions

**What makes a good test here.** Tests drive the application the way a browser
does and assert externally visible behaviour: an HTTP status and body, the
bytes of the configuration file afterwards, the rows in the ledger, the requests
the mock upstream received, and whether a streaming request completed. They do
not call private handlers, inspect internal fields, or assert on layout CSS.
Tests are named as the behaviour they assert, matching the existing suite.

**The seam.** One seam, used for almost everything: the assembled HTTP
application on a real loopback listener, with a temporary configuration file, a
temporary ledger, an in-memory credential store and the existing mock upstream.
Nothing new is invented for the console: this is the same shape as today's proxy
integration tests. The loopback-peer rule is the single exception: its decision
is a pure function of the peer address and is tested directly, because a test
cannot open a real non-loopback connection to the process.

There is no second seam for the CLI/console parity: both call the same
operations module, and the CLI's existing tests keep covering its output.

**Modules under test.** The console HTTP surface (pages, fragments, assets,
guard, headers), the apply/reload engine (through the same HTTP app, asserting
file contents, ledger rows, streaming behaviour and mock-upstream traffic), the
configuration document editing path (through validate/apply, asserting preserved
comments and refused conflicts), the ledger query functions used by Usage with
the temporary ledger, and both language renderings.

**Prior art.** The existing router tests build an application with a fake
backend or a mock upstream and assert status and bodies; the store tests open a
temporary ledger and assert rows; the config tests assert parse and validation
outcomes. The console tests follow those patterns.

**Behaviours the suite must pin.**
- A non-loopback peer gets 404 for every console path, with or without a
  `local_key` configured.
- A write without the custom header, with a foreign `Origin`, with a foreign
  `Host`, or with a non-JSON content type is refused and changes nothing.
- No console response contains a key value; a key added through the console is
  readable only through the credential store, never back through HTTP.
- A page and its fragment render the same fields in both languages; unknown
  `?lang` falls back to English.
- An invalid draft answers 422, names the field, writes no file, and the old
  configuration keeps serving.
- Comments, key order and unknown keys survive a save.
- A draft built from a stale file fingerprint answers 409 and leaves the newer
  file untouched.
- A restart-only field is rendered read-only and an apply that changes it is
  refused by code.
- A configured change applies without dropping a streaming request; that request
  settles with the old snapshot, and a request started afterwards uses the new
  one.
- A removed Account stops taking new work, its in-flight attempt still settles,
  and its ledger rows remain; a new Account is unknown until the probe succeeds.
- Changing a provider's adapter resets its Accounts to unknown.
- The watcher applies an external file change; the console's own save does not
  cause a second apply.
- A failed apply leaves the previous snapshot serving and reports the failure.
- The learned price table and the interval ledger survive an apply.
- Read endpoints make no provider request (the mock upstream's request count is
  unchanged).
- Usage money totals are exact and never sum across currencies; a missing
  window is omitted rather than zero.
- Doctor, probe and key-test actions hit the mock upstream and surface its
  outcome; a timeout answers 504 with a fragment.
- The proxy suite and the CLI suite stay green through the mechanical module
  split.

**Not automated.** Browser layout, theme switching and copy tone are checked
manually with the browser tools during development; the repository has no
JavaScript test framework and this spec does not add one.

## Out of Scope

- Remote or LAN access to the console, logins, sessions, cookies, RBAC and TLS.
- Editing restart-only settings from the console; they remain file-and-restart.
- `lgw init` from the console.
- Websockets, server-sent events and a streaming live tail.
- Per-session spend analytics and a session explorer.
- Custom themes, embedded fonts, languages other than English and Simplified
  Chinese.
- Any change to the `/v1` request, response or authentication contracts,
  including `/v1/usage`.
- Provider calls from read endpoints, provider catalog browsing, and billing
  endpoints.
- A persisted audit log; tracing remains the record.
- Ledger retention changes and money-format changes.
- A JavaScript framework, a Node build step, or frontend unit tests.
- A second configuration source (database overlays, profiles, environment
  overrides) beyond what the file already supports.

## Further Notes

- Three static mockups (editorial ledger, night-watch instrument, product
  console) were built to choose the direction; they are throwaway and are not
  part of the implementation. The chosen direction is recorded above.
- `docs/web-ui.md` was written before the fragment-rendering decision and is
  amended during implementation to describe HTML fragment responses, the
  read-only restart-only fields, and the browser-stored language/theme choices.
- The console is the one deliberate reversal of a documented non-goal; ADR-0001
  records why and the constraints that make it safe.
- The bilingual copy convention of the repository (English primary, Simplified
  Chinese twin) applies to every console page and to this spec.
- Verification before a change leaves the machine: `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, a release
  build, and a manual pass through every console page in both themes and both
  languages.
