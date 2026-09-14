# Decisions made to complete the engine specification

Each entry is a choice `engine-spec.md` needed and the blueprint did not make. They are defaults, not
settled policy: every one can change, and a change means editing the rule, the reference model and the
scenarios together. Entries marked **needs review** affect customers or the rulebook and should be confirmed
before anyone builds on them. Numbers are stable; gaps are decisions that were later folded into rules.

| # | Decision | Why | |
|---|---|---|---|
| D-1 | A void settles every contract at its value under state weights given in `ResolveEvent`, equal weights by default. A binary contract voids at 50¢ a side. The blueprint's "return collateral against cost basis" is replaced. | Cost basis cannot be paid from the pool once proceeds have been released. A buys YES at 40¢ against B's NO at 60¢ (pool $1.00); A sells that YES to C at 70¢ and keeps the 70¢. Refunding cost basis owes C 70¢ and B 60¢: $1.30 from $1.00. Any fixed per-state value conserves, because every state does. Polymarket's 50-50 resolution is the same rule. | **needs review** |
| D-2 | No fees in the v0 engine; fee fields are always 0. | No fee schedule is decided. | |
| D-3 | Self-match prevention defaults to `CANCEL_INCOMING`. | The order being entered is the one the account is acting on; cancelling a resting quote silently is more surprising. | needs review |
| D-4 | An amendment that reprices or increases loses priority and may trade immediately, keeping its order ID. | Cancel-and-replace semantics without forcing a new ID on market makers. | needs review |
| D-5 | Expiry is processed only on `TimeTick`, so its precision is the tick interval (1 ms). | Blueprint §4 makes ticks the engine's clock. | |
| D-7 | Holdings locks are taken only at acceptance and never increased later. | Simple and conservative: a position acquired after an order rests does not retroactively free that order's reservation. | |
| D-8 | Automatic set burns apply only to partition events (disjoint masks covering every state). Events with overlapping books release nothing automatically. | Burns are final and safe; releases against overlapping books are floor advances, which blueprint §7 schedules for phase two. | |
| D-9 | A set burn runs after any fill touching the account in the event, including burning a basket the account minted deliberately and still holds unlocked. | The alternative is tracking which contracts were minted as a basket, which the signed-position model does not do. Money-neutral: the account can mint again. | needs review |
| D-10 | Auto-pull and order-group triggers take effect at the end of the unit, after matching. | Lets `FOK` and capacity checks be decided by a dry run on the book alone; a mid-walk cancellation would make a dry run disagree with the real walk. | |
| D-11 | Rule limits count per whole second of sequencer time. | Deterministic, fixed-size state. Sliding windows can come later without a schema change. | |
| D-12 | Client order IDs are remembered for 24 hours by default; a rejected order still consumes its ID. Each entry expires under the window in force when it was recorded; changing the window affects only later entries. | Blueprint §13 requires a published window; consuming on rejection makes a resend's outcome identical to the original's. A fixed expiry lets an implementation drop expired entries unobservably: when entries were compared against the current window, lengthening it revived entries an implementation had already dropped (found by differential testing of the Rust engine). | needs review |
| D-13 | Order and trade IDs are `(group << 48) + n`. | Unique across groups without coordination. | |
| D-15 | The protection band is measured from the raw best opposite tick, including the account's own orders. | The touch the customer sees; excluding own orders would need a queue walk. | |
| D-16 | A spend cap counts gross price in entered terms, and an order whose cap cannot buy one lot at its effective limit is rejected. | Matches how a stake is understood; the one-lot check is decidable without walking the book. | |
| D-17 | The position limit checks the worst case: position plus every resting order on the same side plus the new order. | Cannot be exceeded by any sequence of fills. | |
| D-18 | Closed-only mode applies to new orders only; resting orders are not altered when it is switched on. | Changing resting orders is what a mass cancel is for. | |
| D-19 | A mass cancel with nothing in scope emits no result. | The gateway acknowledges from the committed input, not from a result. | |
| D-21 | Account 0 is the settlement reserve and receives void rounding remainders. | Blueprint §11's account tree includes a settlement reserve; flooring per account keeps every payout conservative. | |
| D-22 | A parlay is listed under its own two-state event; `ExpireParlay` resolves that event to its losing state. | Reuses resolution rather than adding a second settlement path. | |
| D-23 | Closing a market removes its resting orders. | Blueprint §6: orders must not accumulate in books nobody can trade. | |
