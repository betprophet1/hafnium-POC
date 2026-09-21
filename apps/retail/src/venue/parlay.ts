// The arithmetic and the rules a parlay screen needs, kept away from the screen so they can be tested in Node.
//
// None of this decides anything: the venue refuses a request that breaks its own rules, and its refusal is what the
// person is shown. What is here is only so the app can say what is wrong before sending, and read an answer back.

import type { Market, OrderAnswer, Outcome } from "./types";

/** Legs a parlay may have (core/parlays/src/rules.rs). */
export const MIN_LEGS = 2;
export const MAX_LEGS = 6;

/** Contracts trade in multiples of this, and a request is for at least one multiple. The answer to a request
 * carries the real `lotStep`; this is the venue's default, for the box before the first request. */
export const LOT_STEP = 10;

/** A contract pays one dollar if the parlay comes in. Everything the venue says about money is in whole cents. */
export const CENTS_PER_CONTRACT = 100;

export type Pick = { instrumentId: string; outcome: Outcome; title: string; label: string };

/** Whether this many legs is a parlay the venue would list. */
export function legsOk(count: number): boolean {
  return count >= MIN_LEGS && count <= MAX_LEGS;
}

/** Whether this is a quantity the venue would take: whole contracts, in lots, at least one lot. */
export function quantityOk(quantity: number): boolean {
  return Number.isInteger(quantity) && quantity >= LOT_STEP && quantity % LOT_STEP === 0;
}

/** The markets that may be a leg: still trading, and not itself a parlay — a parlay of parlays is refused. */
export function offerable(markets: Market[]): Market[] {
  return markets.filter((m) => m.state.toLowerCase() === "open" && m.class.toLowerCase() !== "parlay");
}

/** Markets grouped under their event, in the order they arrived. A parlay takes at most one leg per event, so the
 * grouping is what stops a person picking two. */
export function byEvent(markets: Market[]): { eventId: string; title: string; markets: Market[] }[] {
  const events = new Map<string, { eventId: string; title: string; markets: Market[] }>();
  for (const market of markets) {
    const group = events.get(market.eventId) ?? { eventId: market.eventId, title: market.title, markets: [] };
    group.markets.push(market);
    events.set(market.eventId, group);
  }
  return [...events.values()];
}

/** How often the market list is asked for again while somebody is building a parlay.
 *
 * Faster than the markets screen's fifteen seconds, because a stale list costs more here. There it means a card
 * shows the wrong state for a moment; here it means a leg is picked that has already stopped trading, and the
 * request is either refused or — worse, because it looks like nothing — accepted and never quoted, since no
 * responder can price a leg whose book has gone. */
export const MARKETS_REFRESH_MS = 5_000;

/** The picks that are still worth keeping, given the markets as they are now.
 *
 * Rounds roll every few minutes: the market picked a moment ago may have closed, and its event may have been
 * replaced by the next round with new instrument IDs. Dropping those is the honest thing to do — the alternative
 * is a ticket that looks ready and cannot be filled. */
export function stillTrading(picks: Map<string, Pick>, markets: Market[]): Map<string, Pick> {
  const live = new Set(offerable(markets).map((m) => m.instrumentId));
  const kept = new Map<string, Pick>();
  for (const [eventId, pick] of picks) {
    if (live.has(pick.instrumentId)) kept.set(eventId, pick);
  }
  return kept;
}

/** How many contracts an order actually bought.
 *
 * Counted from the fills, which is not the same as reading `status`: an order that filled completely still answers
 * `accepted`, and the executions carry an `accepted` entry beside each `fill`, so summing every execution's
 * quantity counts the same contracts twice. */
export function filledContracts(answer: OrderAnswer): number {
  return (answer.executions ?? [])
    .filter((e) => e.type === "fill")
    .reduce((total, e) => total + (e.quantity ?? 0), 0);
}

/** What a holding pays if every leg happens, in whole cents: a dollar a contract, never the price paid for it. */
export function payoutCents(contracts: number): number {
  return contracts * CENTS_PER_CONTRACT;
}

/** Whole cents as money, the way the venue's own page writes it. */
export function money(cents: number): string {
  return `$${(cents / 100).toFixed(2)}`;
}

/** Seconds until a timestamp the venue sent, never negative, and 0 for one that is missing or unreadable. A window
 * this says is over may have a moment left at the venue, or none: the clocks are different. */
export function secondsUntil(at: string | null | undefined, now: number): number {
  if (!at) return 0;
  const when = Date.parse(at);
  return Number.isNaN(when) ? 0 : Math.max(0, Math.round((when - now) / 1000));
}
