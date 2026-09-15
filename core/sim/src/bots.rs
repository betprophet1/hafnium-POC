//! Synthetic makers and takers (blueprint §16). Ordinary clients of the ingress that decide only from the feed.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::thread;
use std::time::Duration;
use venue_link::view::{InstrumentView, Live, OrderView, View};
use venue_link::wire::{amend, cancel, order, Ids, Ingress};

/// A small seeded generator (splitmix64), so a run can be repeated.
pub struct Rng(u64);

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    pub fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }
}

/// Plans inputs from the view every `interval` and sends them. It plans again only once the feed has shown the
/// effects of everything it sent, so it never acts on a picture older than its own last action.
fn run(ingress: &str, live: &Live, interval: Duration, who: &str, mut plan: impl FnMut(&View) -> Vec<Value>) -> ! {
    let mut ingress = Ingress::new(ingress);
    let mut sent = 0u64;
    loop {
        thread::sleep(interval);
        let inputs = {
            let Ok(view) = live.view.lock() else { continue };
            if view.sequence < sent || view.events.is_empty() {
                continue;
            }
            plan(&view)
        };
        for input in inputs {
            match ingress.send(input) {
                Ok(results) => {
                    for r in &results {
                        sent = sent.max(r["result"]["inputSequence"].as_u64().unwrap_or(0));
                        if r["msg"] == "OrderRejected" {
                            eprintln!("{who}: rejected: {}", r["reason"]);
                        }
                    }
                }
                Err(e) => {
                    // The node may have restarted on a fresh journal; start watching from its sequence again.
                    eprintln!("{who}: {e}");
                    sent = 0;
                    break;
                }
            }
        }
    }
}

fn mine<'v>(view: &'v View, account: u64, instrument: u64, side: Option<&str>) -> Vec<(u64, &'v OrderView)> {
    view.orders
        .iter()
        .filter(|(_, o)| o.account == account && o.instrument == instrument && side.is_none_or(|s| o.side == s))
        .map(|(id, o)| (*id, o))
        .collect()
}

fn nearest_tick(rows: &[u64], target: u64) -> u64 {
    rows.iter().enumerate().min_by_key(|(_, bp)| bp.abs_diff(target)).map_or(0, |(t, _)| t as u64)
}

pub struct Maker {
    pub account: u64,
    /// Contracts quoted at the touch on each side, in lots; each level further out quotes one more multiple.
    pub size: u64,
    /// Price levels quoted on each side.
    pub levels: u64,
    /// Ticks from fair value to each quote; doubled once an event has started.
    pub half_spread: u64,
    pub interval: Duration,
    pub seed: u64,
}

/// Quotes both sides of every open market around a seeded random-walk fair value. In an event with several
/// outcomes the fair values are rescaled to sum to 100¢. Quotes are pulled when an event ends or is suspended.
pub fn maker(ingress: &str, live: &Live, config: &Maker, mut ids: Ids) -> ! {
    let mut rng = Rng::new(config.seed);
    let mut fair: BTreeMap<u64, u64> = BTreeMap::new();
    let account = config.account;
    run(ingress, live, config.interval, "maker", |view| {
        let mut out = Vec::new();
        for event in view.events.values() {
            let open: Vec<(u64, &InstrumentView)> = event
                .instruments
                .iter()
                .filter_map(|id| Some((*id, view.instruments.get(id)?)))
                .filter(|(_, i)| i.state == "OPEN")
                .collect();
            if open.is_empty() || event.resolved {
                continue;
            }
            if event.ended || event.suspended {
                for (id, _) in &open {
                    for (oid, _) in mine(view, account, *id, None) {
                        out.push(cancel(account, *id, oid));
                    }
                }
                continue;
            }
            let spread = if event.started { config.half_spread * 2 } else { config.half_spread };
            let mut quotes = Vec::with_capacity(open.len());
            for (id, inst) in &open {
                let Some(rows) = view.tables.get(&inst.table) else { continue };
                let n = rows.len() as u64;
                if n < 2 * spread + 3 {
                    continue;
                }
                let f = fair.entry(*id).or_insert_with(|| n / 4 + rng.below(n / 2));
                match rng.below(3) {
                    0 => *f = f.saturating_sub(1),
                    1 => *f += 1,
                    _ => {}
                }
                *f = (*f).clamp(spread + 1, n - 2 - spread);
                quotes.push((*id, *inst, rows, *f));
            }
            if quotes.len() > 1 {
                let total = quotes.iter().map(|(_, _, rows, f)| rows[*f as usize]).sum::<u64>().max(1);
                for (id, _, rows, f) in &mut quotes {
                    let target = rows[*f as usize] * 10_000 / total;
                    *f = nearest_tick(rows, target).clamp(spread + 1, rows.len() as u64 - 2 - spread);
                    fair.insert(*id, *f);
                }
            }
            for (id, inst, rows, f) in quotes {
                let top = rows.len() as u64 - 1;
                for side in ["BUY", "SELL"] {
                    // Level k sits k ticks behind the touch and carries k + 1 times the size. A level that would
                    // cross someone else's order is left out, since post-only would reject it.
                    let desired: Vec<(u64, u64)> = (0..config.levels.max(1))
                        .filter_map(|k| {
                            let tick = if side == "BUY" {
                                f.checked_sub(spread + k)?
                            } else {
                                (f + spread + k <= top).then_some(f + spread + k)?
                            };
                            Some((tick, config.size * (k + 1) * inst.lot.max(1)))
                        })
                        .filter(|(tick, _)| {
                            if side == "BUY" {
                                inst.best_ask().is_none_or(|a| *tick < a)
                            } else {
                                inst.best_bid().is_none_or(|b| *tick > b)
                            }
                        })
                        .collect();
                    // Existing orders nearest the touch first, paired with the desired levels in the same order.
                    let mut orders = mine(view, account, id, Some(side));
                    if side == "BUY" {
                        orders.sort_by_key(|o| std::cmp::Reverse(o.1.tick));
                    } else {
                        orders.sort_by_key(|o| o.1.tick);
                    }
                    for (k, &(oid, o)) in orders.iter().enumerate() {
                        match desired.get(k) {
                            Some(&(tick, size)) if o.tick != tick || o.remaining < size => {
                                out.push(amend(account, id, oid, tick, size));
                            }
                            Some(_) => {}
                            None => out.push(cancel(account, id, oid)),
                        }
                    }
                    for &(tick, size) in desired.iter().skip(orders.len()) {
                        out.push(order(
                            account,
                            ids.fresh(),
                            json!({"instrumentId": id, "side": side, "tick": tick, "quantity": size, "flags": ["POST_ONLY"]}),
                        ));
                    }
                }
            }
        }
        out
    })
}

pub struct Taker {
    pub account: u64,
    /// The largest ordinary order, in lots; a sweep is four times this.
    pub max_quantity: u64,
    /// Chance per market per interval of sending an order, in thousandths.
    pub per_mille: u64,
    /// Chance that an order is a sweep three ticks through the touch, in thousandths.
    pub sweep_per_mille: u64,
    pub interval: Duration,
    pub seed: u64,
}

/// Sends immediate-or-cancel orders at the touch at random, with retail-sized quantities and the occasional sweep.
pub fn taker(ingress: &str, live: &Live, config: &Taker, mut ids: Ids) -> ! {
    let mut rng = Rng::new(config.seed);
    let account = config.account;
    run(ingress, live, config.interval, "taker", |view| {
        let mut out = Vec::new();
        for (&id, inst) in &view.instruments {
            let trading =
                inst.state == "OPEN" && view.events.get(&inst.event).is_some_and(|e| !e.suspended && !e.resolved);
            if !trading || !rng.chance(config.per_mille) {
                continue;
            }
            let n = view.tables.get(&inst.table).map_or(0, |rows| rows.len() as u64);
            let buy = rng.below(2) == 0;
            let Some(best) = (if buy { inst.best_ask() } else { inst.best_bid() }) else { continue };
            let sweep = rng.chance(config.sweep_per_mille);
            let lots = if sweep { config.max_quantity * 4 } else { 1 + rng.below(config.max_quantity.max(1)) };
            let tick = match (buy, sweep) {
                (true, true) => (best + 3).min(n.saturating_sub(1)),
                (false, true) => best.saturating_sub(3),
                _ => best,
            };
            let side = if buy { "BUY" } else { "SELL" };
            out.push(order(
                account,
                ids.fresh(),
                json!({"instrumentId": id, "side": side, "tick": tick, "quantity": lots * inst.lot.max(1), "timeInForce": "IOC"}),
            ));
        }
        out
    })
}
