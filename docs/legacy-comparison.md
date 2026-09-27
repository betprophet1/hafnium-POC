# Comparison to the current production venue

How this repo's architecture compares to ProphetX's current production venue — what it structurally fixes,
and what the live system still does that it doesn't yet.

- **New side:** this repo (`prediction-exchange`)
- **Old side:** Confluence and Jira, `betprophet.atlassian.net`
- **Status of the new side:** pre-production

> **Framing.** This isn't apples-to-apples. The old system is live — real order flow, real regulators, real
> payment rails. The new system is pre-production: `core/WORK_PACKAGES.md` shows only a single-node simulated
> sequencer built so far, and the multi-node Aeron Cluster production topology (WP-10) is the last package and
> hasn't been started. Read every "improvement" below as *designed to fix*, not yet *proven in production*.

## Ten dimensions, side by side

| Dimension | Current production | New architecture |
|---|---|---|
| Matching engine | Postgres-polling batch job, 100ms cadence; mid-migration off polling | Deterministic `no_std` engine, pure function of an append-only journal |
| Correctness testing | Bug found by QA load testing, after the fact | Hand-computed scenarios + nightly differential fuzzing + golden replay, gated by CI |
| Single-writer risk | Flagged as unmitigated; no enforced global leader election | Structural: `replicas: 1`, `OnDelete`, Aeron consensus (not yet built) |
| Price representation | Decimal odds + display string — documented one-sided rounding loss | Exact-rational tick ordinals, integer cents |
| Blast radius | One shared Reporting DB took down auth + trading (#cftc-inc-7) | One database/schema per service, enforced as a privilege |
| Disaster recovery | Cross-region RDS promotion; Kafka has no cross-region replication; untested in prod | Journal replay + canonical snapshots; also untested in prod |
| CFTC reporting | Built: Part 16 FIXML, daily back-office CSVs, in review with counsel | Out of scope in the repo's own interface catalogue ("outside") |
| Market surveillance | Working Kafka → Redshift → Nasdaq SMARTS pipeline | No component built; vendor boundary only |
| Payment rails | Live: ZeroHash, PayNearMe, Aeropay, Plaid | Treasury is a test-only dummy with a fake bank |
| Oracle / settlement | Settles real sports outcomes today, manually | Entirely a synthetic test double; real feeds are phase 3 |

Detail and citations for each follow.

## Where the new architecture fixes a real, current problem

Each of these ties a specific new-architecture decision back to a specific documented failure or limitation
in the live system.

### 1. Matching-engine correctness and determinism

The current matching path is a Postgres-polling batch job — `matchingWagers` polls every 100ms — not a
real-time engine, and it has a live, documented correctness bug: **SSE-2328**, a permanently crossed order
book caused by "aggress-once" semantics racing an async deduct/confirm step. It's reproducible under load and
hasn't hit production yet. The team is also mid-migration off polling entirely because, in their own words, a
single instance can't keep up and the job table can't be sharded for horizontal scale.

The new engine is a `no_std` pure function of an append-only journal: deterministic, replay-verified
byte-for-byte every night via golden replay, with scenario tests computed by hand rather than derived from
any implementation — so a test can't accidentally confirm a bug.

| | Current | New |
|---|---|---|
| | Poll-based matching; crossed-book bug open; scaling by adding pollers, not partitioning | Journal-driven deterministic engine; independent scenario oracle; nightly replay diff |

Sources: Confluence 92471297 (Trade Service Technical design), 1566277633 (SSE-2328), 685735945 (Matching
Deduce Feature Architecture); `spec/engine/scenarios/README.md`; `AGENTS.md`.

### 2. The single-matching-engine risk becomes a design constraint, not a hope

The old system's own DR document states the risk plainly: *"Two matching-engine instances against one order
book is a catastrophic outcome for an exchange"* — and no global leader-election mechanism enforcing that is
actually implemented; it's listed as something to verify in a *future* failover test. The new architecture
makes single-writer structural rather than operational: the core deploys as `replicas: 1` with
`updateStrategy: OnDelete` so a sequencer group never rolls on its own, with Aeron Cluster consensus intended
for the multi-node production topology.

Caveat: that multi-node topology (work package WP-10) is not built yet — today there's only a single
in-process simulated sequencer. This is a better-designed answer to a named risk, not yet a tested one.

Sources: Confluence 1580335145 (Compute failover for production EKS to us-west-2); `deploy/README.md`;
`core/WORK_PACKAGES.md` WP-10.

### 3. Numeric correctness in price representation

The current `/v4` API carries price as decimal odds plus a preformatted display string. The blueprint
documents this as a systematic, one-sided rounding-error source — decimal odds don't terminate for many
American-odds values, and the loss lands hardest on favorites and high-volume prices. The new engine works
only in exact-rational tick ordinals and integer cents; a quoted price is "a label for a tick, never an input
to a calculation."

Source: `blueprint/prediction-exchange.html` §6, §13.

### 4. Blast-radius isolation between services

**#cftc-inc-7** (20 Sep 2026): an unbounded parlay-leg-validation query with no `LIMIT`, added for mobile,
pegged the shared Reporting DB to ~99% CPU — which cascaded into Nova, Mothership/Auth (login failures), and
trade placement. One frontend feature took down authentication and trading because they shared a database.

The new architecture gives every service its own database, role, and schema, enforced as a privilege rather
than a convention, with a hard network-policy boundary limiting what can reach the core and each service, and
a documented, independent degrade-on-DB-loss behavior per service (refuse writes with 503, keep serving reads
from an in-memory working copy, leave the Kubernetes Service on DB recovery).

Sources: Confluence 1716650026 (#cftc-inc-7); `core/store/decisions.md` S-3, S-11, S-16, S-19.

### 5. Disaster recovery built on replay, not just replica promotion

**#cftc-inc-2** (13 Aug 2026): a `terraform destroy` against a "QA" failover-test cluster deleted a shared IAM
role, because a prior drift-remediation import had silently coupled it to production. Recovery was dominated
by Debezium replication-slot contention the team had already flagged days earlier and not tracked. Notably,
the team *considered and rejected* failing over to us-west-2 mid-incident, because Debezium would fail
identically there — the DR path was judged non-viable in the moment it was needed. Kafka/MSK has no
cross-region replication today; DR activation is manual and has only been tested in Staging.

The new architecture's journal-as-truth model — canonical snapshots, replay-based recovery, reserved (not
just provisioned) standby capacity — is a structurally different recovery story, though it hasn't been
exercised under a real failure either; its own docs say object-storage shipping of cut journals "is not built
yet."

Sources: Confluence 1620770854 (#cftc-inc-2), 1069973605 (PT-321 Hot/Warm Failover Testing);
`core/node/README.md`.

### 6. Regulatory audit trail as structural, not retrofitted

The current CFTC compliance stack (Debezium → Kafka → SMARTS transform, Redshift surveillance store, Part 16
/ daily back-office reports) was built as a pipeline layered onto a system not designed for it — and
**parlays currently bypass the CLOB entirely and are missing from the SMARTS feed**, an acknowledged
compliance gap. The new architecture bakes event sourcing and hash-chained edge audit logs in from day one,
plus structural money-mode isolation (fake / sandbox / live, enforced by four independent locks).

Caveat: the SMARTS export and CFTC reporting interfaces are explicitly marked `outside` the new system's own
interface catalogue — better bones today, not a working replacement.

Sources: Confluence 1451491909 (Product Brief: Parlay CLOB Placement Proposal and Audit Trail Compliance
Strategy); `interfaces/audit/README.md`; `interfaces/catalogue.toml` #18, #19.

### 7. Fee and maker/taker logic native to the engine, not bolted on

The current maker/taker fee model and Add-Liquidity-Only order are being built as a bolt-on: a separate Fee
Resolution Engine service, a new `fee_transactions` table, and a documented risk that mid-flight fee-model
changes can orphan fee transactions and desync ledgers. The new engine treats fee computation, maker/taker
roles, and rebates as native, deterministic parts of the fill algorithm, with per-account/instrument/event
overrides designed in from the start.

Sources: Confluence 1481998337 (Maker/Taker Fee Model & Add Liquidity Only order); `spec/engine/decisions.md`
D-2, D-26, D-27.

### 8. Testing discipline as a CI gate, not a QA pass

A crossed-book bug found late, and a query that took down auth and trading because "no load testing had been
done against realistic high-volume inputs," are both process failures as much as design ones. The new repo's
`./check` blocks any engine change unaccompanied by a passing, hand-computed scenario, plus 300+-seed
differential fuzzing against a reference model — a structurally stronger pre-production bar.

Sources: Confluence 1716650026; `AGENTS.md`; `core/WORK_PACKAGES.md`.

## Where the current architecture covers ground the new one hasn't reached

The new repo has the better skeleton in several places above — but a live exchange is mostly built out of the
things below, and almost none of them exist yet in this repo.

1. **It's running.** Real order flow, real market-maker and FIX relationships today (the FIX Engine brief
   targets over 50% of volume via FIX by Q2 2027, implying live relationships now), real regulatory standing.
   The new system has none of this yet — it's pre-production. *(Confluence 1425309743)*

2. **CFTC reporting formats.** Actual FIXML/XSD Part 16 report structure (decimal-padding, full-overwrite
   correction semantics) and daily back-office CSV generation are built and in review with a compliance
   consultant. None of this format-specific knowledge exists in the new repo; its "Reporting → CFTC"
   interface is explicitly out of scope, a vendor boundary rather than a component.
   *(Confluence 1190461459, 1053786169)*

3. **Nasdaq SMARTS integration.** A working, if bolted-on, transformation service reshapes wager data into
   SMARTS' schema, and a dedicated Redshift ingestion pipeline serves as the system of record for CFTC audit
   today. The new repo has no analogue — market surveillance export is marked `outside` in its interface
   catalogue. *(Confluence 780435471, 867172362)*

4. **Live payment rails.** ZeroHash (crypto/USDC), PayNearMe, Aeropay, and Plaid are integrated today, with
   real operational history — including a reconciliation incident where a ZeroHash USDC deposit wasn't
   reflected in wallet, statement, or Nova after on-chain confirmation. The new treasury is explicitly a
   test-only dummy with a fake bank; real banking and payment/KYC adapters are marked "outside" or "planned."
   *(Jira SSE-2161; `core/treasury/README.md`; `interfaces/catalogue.toml` #16, #21)*

5. **Manual risk, fraud, and KYC operations.** Withdrawal review lanes, GeoComply/IDComply geolocation
   checks, and Slack-alert-driven fraud triage form a whole operational apparatus today. This is an
   organizational-process gap for the new system as much as a code one — nothing in the repo addresses it,
   and nothing should be expected to; it lives with Risk & Fraud, not in the engine.
   *(Confluence 1635450897, 1658716356)*

6. **Live-tuned business parameters.** Specific, calibrated policy values exist only in the current system's
   config and its Nova admin tool: a $10M default exchange position ceiling, a $25K soft accountability
   alert, per-market-type netting rules (moneyline nets by outcome, spread/total nets by line). The new engine
   has the *mechanism* for rule limits and overridable fee schedules, but none of the institutional knowledge
   behind the actual numbers. *(Confluence 1064468714, 819134627)*

7. **B2B / DCM clearing maturity.** The current B2B partner service and market-maker API already serve
   external volume. The new system's market-maker-facing interfaces (binary gateway, fan-out, FIX, drop copy)
   are all marked `after-mm-review` in its own catalogue — earlier-stage, with wire formats not yet frozen
   against real firms. *(Confluence 738820112; `interfaces/catalogue.toml` #2–#5)*

8. **Real oracle and settlement.** The old system settles real sports outcomes today, however manually. The
   new system's oracle is entirely a synthetic test double — two dummy sources, a referee fallback, an
   auto-approver on test credentials. Real oracle feed adapters are explicitly phase 3, not built.
   *(`core/sim/README.md`)*

9. **Disaster recovery exercised in practice.** The current DR runbooks have actually been tested, in
   Staging, with real postmortems to learn from. The new snapshot/replay tooling exists as "building blocks,"
   by its own documentation's description, not yet wired into any real failure drill or preview environment.
   *(Confluence 1069973605)*

> **A gap both sides share.** Parlays aren't first-class CLOB / audit-trail citizens on either side. On the
> old system this is an acknowledged compliance issue — parlays bypass the CLOB and are absent from the
> SMARTS feed. On the new system, a parlay is listed under its own two-state event and RFQ-quoted before
> becoming an ordinary market (decision D-22) — architecturally cleaner, but not yet validated against actual
> CFTC review either. Worth tracking as one open question, not two separate ones.

## Presentation: American odds vs. exact ticks

A follow-up question worth answering precisely, since it looked at first like a customer-facing regression:
the engine works only in tick ordinals and exact rationals, but ProphetX customers read American odds. Is
that a presentation problem?

**No — the conversion is the design, and it's already built.** The gateway is the translation boundary in
both directions (`core/gateway/src/price.rs`). A client can send a price as `tick`, `probability`, `cents`, or
`american` — each form is parsed into an exact rational and matched against the instrument's ladder; a price
that doesn't land exactly on a row is refused with `PRICE_NOT_ON_LADDER`, never silently rounded. Every price
quoted back out is a single `PriceQuote` object carrying *all four forms at once* — tick, exact probability,
cents, and American odds — computed straight from the ladder row, never from a stored decimal.

So a screen showing `-110` keeps working exactly as it does today. What changes is underneath: `-110` is
never round-tripped through decimal odds and back — the documented source of the current system's one-sided
rounding loss on favourites — it's looked up as the exact rational 11/21, and every displayed form is a
derivation of that, never independently stored or independently authoritative.

Sources: `core/gateway/src/price.rs` (`parse()`, `quote()`, `american()`); `interfaces/gateway/README.md`
"Prices"; `interfaces/gateway/openapi.yaml` `PriceQuote`.

**The one real caveat: not every tick has a clean American-odds label.** `american()` returns null when a
tick's probability isn't a whole American-odds number — which can't happen today, because every price is
*entered* in odds terms, but can happen on the new ladder-based system depending on which ladder type an
instrument uses:

- An **odds ladder** (pairing +A/-A, closed under complement) gives every tick a clean American-odds label by
  construction — no issue.
- A **cent ladder** (the default for parlays, and for deep out-of-the-money binaries) will have ticks with no
  whole-number odds equivalent at all — the API returns `american: null`, and a UI has to handle that rather
  than assume odds are always present.

Which ladder type each instrument class gets is explicitly flagged as still open in the blueprint. Not an
issue for the straight sports markets ProphetX customers already trade, as long as those stay on odds
ladders — but a real open question for parlays/exotics worth confirming with product before launch.
*(`blueprint/prediction-exchange.html` §21, "Price ladder per instrument class")*

**Why this matters beyond one question.** The current product roadmap already has a cluster of initiatives
fighting this exact problem one format at a time: IN-88 shipped a percentage/cent display mode; IN-89
(dual-inline multiplier display) is in progress; IN-137 (add multiplier & decimal odds type) is still in
Discovery; IN-135 (odds ladder refactor) is queued; a standalone odds-converter tool (IN-101) was scoped and
marked won't-do. On the new architecture, decimal odds and a multiplier display are the same rational the
engine already carries — `quote()` already returns probability and cents together; adding a `decimal` field
is arithmetic on a value already in hand, not a new subsystem. Worth treating "multi-format odds" as
effectively solved by the price model, rather than as a queue of separate initiatives.

## Other product-level differences

Beyond infrastructure, a few product mechanics are worth calling out on their own — one where the new side is
already ahead of a current ask, one explicit conflict with a current ask, and several where the two are
aligned but at different stages.

### Subaccounts: the new repo has already built what IN-120 is still asking for

The current backlog carries **IN-120 · Parent/Child Account Structure** as a High-priority, "To Do"
initiative. Two PRs merged into the new repo's `main` on 2026-09-25 already implement this: an account can
name another as its **master** (decision D-33), self-match prevention widens to the whole family — a master
and its subaccounts, or two subaccounts of the same master, can never trade against each other — while risk
(limits, positions, margin, fee overrides) stays keyed per subaccount, because keeping risk separate is the
entire point of having subaccounts. An internal-transfer primitive (D-34) moves cash between a master's
accounts via the treasury's existing reclaim/grant machinery, with per-key IP allow-lists (G-23) as a
related, separately-shipped piece.

Caveats, so this isn't overstated: D-33 itself is still flagged **needs review** — the owner confirmed the
requirement but not yet the exact mechanism (a new engine command, `SetMasterAccount`); WS/FIX-side
IP-allow-list enforcement (G-23) covers the REST gateway path only so far; and real outbound mail for
security notices (G-24, Amazon SES) is decided but not implemented, blocked on SES production access.

Sources: `spec/engine/decisions.md` D-33, D-34; `interfaces/gateway/decisions.md` G-21–G-25; Jira IN-120.

### Margin trading: an explicit conflict, not a gap

**IN-58 · Margin Trading** sits on the current backlog as a "To Do" initiative. The new architecture's
full-collateralization model isn't an oversight that leaves margin for later — the blueprint chooses it
specifically to avoid Part 39's margin-methodology, guaranty-fund, and default-waterfall obligations, and
treats leveraged or uncapped payoffs as a different company's product entirely. If margin trading is still a
live product goal, that's a direct strategic conflict with the new venue's regulatory posture, worth
surfacing to whoever owns both roadmaps rather than quietly falling out of scope.

Source: `blueprint/prediction-exchange.html` §8, §22.

### Other current initiatives, read against the new architecture

| Initiative | Current status | Read |
|---|---|---|
| IN-55 Block Trading | To Do | **Deferred, not conflicting** — the new FIX dialect already lists block trading as explicitly "Day 2," same direction as today, just later. |
| IN-119 Display Queue Position | To Do | **Cheap on new side** — price-time priority with an intrusive per-tick FIFO is already the book's data structure; exposing an order's position is a read, not new machinery. |
| IN-123 User-level total resting-order-value limits | Selected for Development | **Cheap on new side** — sequenced rule limits and position limits (E-9.4, E-9.7, D-11) already exist as first-class engine state; this looks like one more limit type on existing machinery. |
| IN-118 New TIF types (GTC/GTD/GTT) | Discovery | **Close already** — order expiry via `expiresAt` and sequenced `TimeTick` already gives GTC and GTD; GTT needs confirming but the primitive is the same one. |
| IN-117 RFQ Cash Out / IN-116 Phase 2 Cash Out | To Do | **Mechanism exists, product doesn't** — flattening a position by trading the opposite side works today on the new CLOB, same idea as the current phase-1 cash-out; a dedicated one-tap close endpoint isn't specified yet. |
| IN-60, IN-141 Universal API (OAuth) | In Progress / To Do | **Partial gap** — the new gateway's key model (G-1–G-13: scoped permissions, per-key IP allow-lists, split flood/rule limits) covers most of what later phases want, but it's bearer-token, not OAuth — phase 2's OAuth modal has no equivalent yet. |
| IN-128 Crypto event contracts, IN-95 Non-sports contracts, IN-121 Sport perps (no leverage) | To Do / Discovery | **Roadmap-aligned** — all map onto the blueprint's instrument classes (binary/categorical/scalar/parlay) and its capped-linear-contract discussion as the no-leverage substitute for perpetuals; none are built yet on either side. |
| IN-105 SOC2 Certification | To Do | **Org process** — not a code gap either side closes by itself, though the new side's hash-chained audit logs and money-mode isolation are the kind of control a SOC2 audit wants evidence of. |

Roughly a third of the full 142-item "IN" backlog (B2B white-label integrations, payment-provider rollouts,
UI redesigns, marketing, rewards) is orthogonal to this comparison entirely — product and partnership work
that neither depends on nor blocks the rearchitecture.

## Postmortems: what changes, what doesn't

The old system's incident archive runs back to 2022 (35+ postmortems). A curated set below, chosen to span
distinct failure mechanisms rather than severity — each with the old root cause and a grounded read on
whether the new architecture's design removes that mechanism, narrows it, or leaves it untouched.

### Addressed — the failure mechanism doesn't exist in the new design

**inc#81 · Wallet/Trade async error swallowing** (2026-03-04, P1, 4h downtime, ~$2M locked, 156 users, 9 SPs).
Wallet returned `422 Insufficient Funds` during wager submission; Trade treated it as non-fatal and marked the
job `submitted` anyway. Thousands of these zombie jobs then flooded the confirmation pipeline's retry queue
and starved legitimate activations platform-wide. Root cause: a cross-service async call whose failure mode
was misclassified, with no fail-fast for a known-bad response. New architecture: cash is checked and locked
at order acceptance *inside* the single deterministic engine (holdings locks and reservations, E-11.1/E-11.2)
— there's no separate wallet service to call, so there's no answer from it to misinterpret. *(`spec/engine/engine-spec.md` §11)*

**inc#76 · Parlay quotes lost to a single Pusher cluster** (2026-01-16, P1, ~1h40m, all parlay SPs down). All
parlay websocket quoting depended on one third-party Pusher cluster in one physical region, with no automated
failover. When that cluster silently stopped delivering messages, every provider went dark until engineers
manually cut over to a different Pusher region — and Pusher's own status page didn't confirm the outage until
*after* the fix. New architecture's market-data and parlay-quote fan-out is first-party: the venue's own
gateway processes serve `ws_public`/`ws_private` directly and can run as several independent instances (G-13,
"several gateways"). *(`deploy/README.md` "Several gateways")*

**incident-23 · Mobile's only cancel path broke with DMS sync** (2023-07-07). An AWS network issue broke DMS
replication into the reporting read model, and mobile users' *only* way to cancel a wager was through the "My
Bets" tab — itself dependent on that same broken sync. New architecture: a cancel is a command sent straight
to the sequencer group over the gateway (interface 7), never routed through a downstream read replica.
*(`interfaces/catalogue.toml` #7)*

**Redis OOM from rate-limit tracking** (2024-10-25, production Redis at 100% memory, MM APIs 429'd
platform-wide). A couple of high-volume API users pushed a shared production Redis (used for rate-limit
tracking) to full memory, breaking rate limiting for every institutional market maker on the platform. The
new gateway's own decision record calls this exact tradeoff out by name: sharing rate-limit buckets in Redis
"was weighed and not taken: it would put a network round trip and a new failure mode on every order for the
one limit that multiplies." *(`interfaces/gateway/decisions.md` G-13)*

**Recurring CDC/Debezium collector overload** (incident-20, 2023-03-31; incident-26, 2023-07-20; Debezium
stability incident, 2026-07-03). Three incidents, three years apart, the same shape: a burst of market or
wager updates overwhelms a Debezium/Kafka collector, and liquidity or wager-status updates go stale by 30+
seconds. New architecture's market data path is push-based straight off the journal (sequencer → fan-out /
conflation, interface 8), watermarked by sequence number, not CDC polling a database table. Caveat: ClickHouse
and Valkey downstream readers are still "not started," so this is a claim about the live market-data path
specifically, not full parity with every current downstream consumer yet. *(`interfaces/catalogue.toml` #8, #26, #27)*

### Not addressed — the same risk class still applies

**#cftc-inc-2 & Incident #47 · IaC blast radius** (2026-08-13, P1, 3h; 2024-02-15, 4h). A `terraform destroy`
against a "QA" cluster deleted a shared production IAM role; separately, an ArgoCD Application CRD upgrade
deleted the ingress controller platform-wide. The new architecture's services tier still runs on
Kubernetes/Helm/ArgoCD — this category of mistake remains structurally possible. The one partial mitigation:
the environment-per-PR system uses an `AppProject` that hard-restricts preview deploys to `venue-pr-*`
namespaces, a narrower blast-radius boundary than whatever coupled "QA" to production in #cftc-inc-2 — a
policy improvement worth having, not something the rearchitecture specifically targeted, and untested against
this exact failure. *(`deploy/argocd/project.yaml`)*

**#ss-inc-4 · Silent cert-rotation failure** (2024-09-19). An expired CloudFront certificate wasn't caught
because GitOps-driven rotation failed silently, forcing a manual fix in the AWS console. New deploy uses
cert-manager instead, a more automated mechanism — but this is a tooling difference the rearchitecture
doesn't target. *(`deploy/README.md` "A certificate")*

**incident-30 · Suspended user still able to trade** (2023-08-31). A check-then-act race let a user place a
bet shortly after being suspended in Nova, because the auth cache hadn't caught up with the suspension yet.
New architecture bounds and documents this window instead of leaving it open-ended: key/session revocation is
cached 5 seconds and takes effect within 10 (G-7). Smaller and specified rather than ad hoc — but not
literally zero. *(`interfaces/gateway/decisions.md` G-7)*

## Closing the four gaps: readers vs. writers

The four things production does that the new architecture doesn't yet touch — CFTC reporting, market
surveillance, payment rails, oracle/settlement — split into two different shapes of work, not one. None of
the four has its internal seam built yet: all four sit at `outside` or `planned` in the repo's own interface
catalogue.

### Reader-shaped: CFTC reporting & market surveillance

Both interfaces are marked `status = "outside"` in `interfaces/catalogue.toml` — meaning the wire format is
the regulator's or Nasdaq's to define, not this repo's. The internal piece actually needed is a journal
reader: watermark against sequence numbers, apply exactly once, never write back — the same shape already
planned for the ClickHouse history loader (interface 26) and the Valkey live-state projector (27), neither of
which is built yet either. The real work is (1) the watermarked-reader mechanism itself, and (2) the
domain-specific transform from the engine's canonical tick-ordinal/YES-terms representation into whatever
schema the regulator or vendor wants (the old system's FIXML CMPOS/EMKTDATA/etc., decimal-padded,
full-overwrite correction semantics) — real engineering, not a byproduct of having a journal.

CFTC reporting also needs the retention/archive writer (interface 29 — Object Lock, signed chained manifests)
and the hold register every deletion must consult first (interface 30), both `planned`. The audit README
already admits today's implementation is "a chained manifest rather than a signed one," with no
hold-register enforcement yet — a compliance-grade write-once archive, not a passive read.

Sources: `interfaces/catalogue.toml` #18, #19, #26, #27, #29, #30; `interfaces/audit/README.md`.

### Writer-shaped: payment rails & oracle/settlement

Both need to *write into* the sequencer group as new sequenced commands — a confirmed deposit becomes a
`SleeveAdjust GRANT`, a settled game becomes a `ResolveEvent`. The seam is already designed: interface 21
(Treasury ↔ payment/banking/KYC adapters) and interface 15 (Oracle adapters → operations consoles) are each
specified as a **Rust trait plus conformance suite** (both `planned`, neither built) that the dummy treasury
and dummy oracle already implement one side of. Real integration means writing a second implementation of the
same trait — calling ZeroHash/PayNearMe/Aeropay/Plaid, or a licensed sports-data feed — and passing the same
conformance tests the dummy passes. The hard part is already solved: the outbox-relay pattern (interface 25,
`status = "next"`) with idempotent command IDs is already exercised end-to-end for the dummy treasury's
reclaim/grant flow. What's missing is the adapter behind the trait, not the plumbing to the engine.

Oracle integration has one more piece beyond the data feed: the resolution console (interface 24, `planned`)
— human two-of-three sign-off and dispute handling — because the blueprint treats a single data vendor as "a
settlement dependency, and therefore a regulatory one" and won't let a feed resolve a real market unattended.
Both adapters also have to participate in the blueprint's money-mode isolation (asserting their source
matches the account's declared fake/sandbox/live mode) — a design requirement already specified even though
nothing implements it yet.

Sources: `interfaces/catalogue.toml` #15, #16, #17, #21, #24, #25; `core/treasury/README.md`;
`blueprint/prediction-exchange.html` §11, §16.

### Summary

| Gap | Catalogue status | Shape | What's actually missing |
|---|---|---|---|
| CFTC reporting | 19: outside | reader + archive writer | Watermarked journal reader (doesn't exist), regulator-specific schema transform, Object Lock archive (29) + hold register (30), both planned |
| Market surveillance | 18: outside | reader | Watermarked journal reader (doesn't exist), SMARTS-specific schema transform, Nasdaq network peering |
| Payment rails | 16: outside, 21: planned | writer | The trait itself (21, not built), a real adapter behind it, inbound webhook handling, KYC/fraud program (largely manual on the old side too) |
| Oracle / settlement | 17: outside, 15/23/24: planned | writer + human console | The trait itself (15, not built), a licensed data-vendor adapter, the resolution console UI (24, two-of-three sign-off) |

> **Working assessment.** For the **writer-shaped** pair (payment rails, oracle/settlement), we can't
> meaningfully scope the adapter work until we have access to the current system's existing integration code
> — the ZeroHash/PayNearMe/Aeropay/Plaid client logic and the sports-data vendor feed handling — since that's
> what a real trait implementation has to be built against, credentials and provider quirks included.
>
> For the **reader-shaped** pair (CFTC reporting, market surveillance), the same access would help too —
> particularly the exact FIXML report generation and SMARTS transformation logic already built on the old
> side — but we're comfortable that this fits within the existing architecture as designed. It's a watermarked
> journal reader plus a schema transform, the same shape as the already-planned ClickHouse and Valkey readers;
> no major architectural change is needed to accommodate it.

## Sources

New-architecture claims are drawn from this repo: `blueprint/prediction-exchange.html`, `AGENTS.md`,
`core/WORK_PACKAGES.md`, `spec/engine/decisions.md`, `interfaces/gateway/decisions.md`,
`core/store/decisions.md`, `interfaces/catalogue.toml` (all 30 interface entries),
`core/gateway/src/price.rs`, `core/treasury/README.md`, `interfaces/audit/README.md`, `deploy/argocd/`, and
component READMEs under `core/` and `interfaces/` — including commits on `origin/main` (subaccounts, PRs
#29/#31) not yet pulled into the local checkout at the time of writing.

Current-architecture, product-backlog, and postmortem claims are drawn from Confluence and Jira on
`betprophet.atlassian.net` (cloud ID `557de1d4-7bef-47c8-8b92-64d985a947fc`; project `IN` for initiatives;
incident postmortems mainly in the `BETPROPHET` and `DC` spaces). No changes were made to Jira or Confluence
in producing this review.

This document was also published as a standalone artifact for easier reading and sharing:
https://claude.ai/artifact/PuLq7cDf5uRDcNpaASdmFJ (private; grant access from its Share menu to hand it to
someone else).
