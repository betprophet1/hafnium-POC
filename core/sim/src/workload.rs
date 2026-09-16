//! A workload with the shape of a real venue: hundreds of books, most of them quiet, a few busy, orders that
//! actually trade, and amendments and cancellations of orders that are really resting.
//!
//! It drives a node's ingress directly, so nothing stands between the generator and the core. The node writes its
//! journal as it commits, and that journal is the artefact worth keeping: `venue-node bench` replays it for
//! per-message percentiles, and replaying it twice must produce identical results.
//!
//! What it is not: a test of the gateway. There are no sessions and no feed readers here, on purpose.

use crate::bots::Rng;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use venue_link::catalogue::TICK_TABLE;
use venue_link::submit::{Pending, Submit};
use venue_link::wire::{self, Error, Ids};

pub struct Config {
    pub books: u64,
    /// The few books that take most of the flow, then the ones that take some; the rest are the long tail.
    pub hot: u64,
    pub warm: u64,
    pub accounts: u64,
    /// The first few accounts, which quote for a living: most of the flow is theirs, and the depth is all theirs.
    pub makers: u64,
    /// What an operator sets, and a run of any length needs both (`SetGroupParameters`). Without an order limit
    /// beneath the engine's order slab, a venue whose books fill up fail-stops on the next order instead of
    /// refusing it (E-9.3 check 17). Without a client order ID window short enough for the order rate, the
    /// duplicate table fills, because nothing leaves it until its window is up (E-8.2, E-9.2, D-12).
    pub order_limit: u64,
    pub id_window_seconds: u64,
    /// Inputs given to the node before waiting for the first one's results. One at a time measures the generator
    /// waiting on round trips as much as it measures the venue; several at a time measures the venue.
    pub depth: usize,
    pub seconds: u64,
    pub seed: u64,
}

struct Book {
    instrument: u64,
    /// Where this book trades, so the books are not all priced alike.
    fair: u64,
    /// The orders believed to be resting here, with the account that owns them: only the owner may amend or
    /// cancel one, and an amendment is only interesting if it lands on an order that is really there.
    resting: Vec<(u64, u64)>,
}

/// How many markets one customer follows. Without a limit, every account eventually holds a position in every
/// book, and the venue's holdings become the cross product of accounts and instruments — which is both far more
/// than a real venue holds and, at these sizes, more than the engine is built to hold.
const FAVOURITES: u64 = 3;

#[derive(Default)]
struct Counts {
    sent: u64,
    accepted: u64,
    rejected: u64,
    fills: u64,
    amended: u64,
    cancelled: u64,
}

/// Only to time the run and to date GTD orders; nothing in the venue reads this clock.
#[allow(clippy::disallowed_methods)]
fn now() -> Instant {
    Instant::now()
}

#[allow(clippy::disallowed_methods)]
fn wall_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

fn cent_table(ids: &mut Ids) -> Value {
    let ticks: Vec<Value> =
        (0..99).map(|t| json!({"tick": t, "numerator": t + 1, "denominator": 100, "complement": 98 - t})).collect();
    wire::command("DefineTickTable", ids.fresh(), json!({"tickTableId": TICK_TABLE, "ticks": ticks}))
}

/// Lists every book and funds every account. Sent one at a time, like everything else here.
fn setup(ingress: &Submit, ids: &mut Ids, c: &Config, counts: &mut Counts) -> Result<Vec<Book>, Error> {
    send(ingress, cent_table(ids), counts)?;
    let limits = json!({
        "activeOrderLimit": c.order_limit, "clientOrderIdWindowSeconds": c.id_window_seconds
    });
    send(ingress, wire::command("SetGroupParameters", ids.fresh(), limits), counts)?;
    for account in 1..=c.accounts {
        let fields = json!({"accountId": account, "deltaCents": 1_000_000_000u64, "reason": "GRANT"});
        send(ingress, wire::command("SleeveAdjust", ids.fresh(), fields), counts)?;
    }
    let mut books = Vec::new();
    for i in 0..c.books {
        let event = 900_000 + i;
        let instrument = event * 10;
        let fields = json!({"eventId": event, "stateCount": 2, "scheduledStart": 0});
        send(ingress, wire::command("DefineEvent", ids.fresh(), fields), counts)?;
        let listing = json!({
            "instrumentId": instrument, "eventId": event, "instrumentClass": "BINARY", "stateMask": 2,
            "tickTableId": TICK_TABLE, "lotStep": 1, "allocationPolicy": "PRICE_TIME", "positionLimit": 0, "legs": []
        });
        send(ingress, wire::command("ListInstrument", ids.fresh(), listing), counts)?;
        let open = json!({"instrumentId": instrument, "state": "OPEN"});
        send(ingress, wire::command("SetMarketState", ids.fresh(), open), counts)?;
        // 20c to 79c, so the books are spread across the ladder rather than stacked on one tick.
        books.push(Book { instrument, fair: 20 + (i % 60), resting: Vec::new() });
    }
    Ok(books)
}

/// One input, waited for. Setup is sequential on purpose: a market has to exist before anything trades on it.
fn send(submit: &Submit, input: Value, counts: &mut Counts) -> Result<Vec<Value>, Error> {
    counts.sent += 1;
    submit.send(input)
}

/// Which book this input is for: most of the flow to the hot books, some to the warm, a little to the long tail.
fn pick(rng: &mut Rng, c: &Config) -> usize {
    let tail = c.books.saturating_sub(c.hot + c.warm).max(1);
    let which = rng.below(100);
    let book = if which < 70 {
        rng.below(c.hot.max(1))
    } else if which < 90 {
        c.hot + rng.below(c.warm.max(1))
    } else {
        c.hot + c.warm + rng.below(tail)
    };
    (book % c.books) as usize
}

/// A new order. A market maker quotes passively, in size, spread across the ladder, which is what gives a book
/// depth; the crowd mostly crosses, which is what makes it trade.
fn new_order(rng: &mut Rng, ids: &mut Ids, book: &Book, account: u64, maker: bool) -> Value {
    let buy = rng.chance(500);
    let tick = if maker {
        if buy {
            book.fair.saturating_sub(1 + rng.below(15))
        } else {
            (book.fair + 1 + rng.below(15)).min(97)
        }
    } else if rng.chance(600) {
        // Priced through the quotes, so it trades.
        if buy {
            (book.fair + 1 + rng.below(3)).min(97)
        } else {
            book.fair.saturating_sub(1 + rng.below(3))
        }
    } else if buy {
        book.fair.saturating_sub(1 + rng.below(8))
    } else {
        (book.fair + 1 + rng.below(8)).min(97)
    };
    let quantity = if maker { 10 + rng.below(40) } else { 1 + rng.below(10) };
    let mut fields = json!({
        "instrumentId": book.instrument, "side": if buy { "BUY" } else { "SELL" },
        "tick": tick, "quantity": quantity
    });
    // A few orders carry an expiry, so orders leave by expiring as well as by trading and being cancelled.
    if rng.chance(80) {
        fields["timeInForce"] = json!("GTD");
        fields["expiresAt"] = json!(wall_ns() + (1 + rng.below(30)) * 1_000_000_000);
    }
    // A venue that is never sent anything wrong is not being tested: one order in a hundred is off the ladder,
    // which E-9.3 must reject rather than take.
    if rng.chance(10) {
        fields["tick"] = json!(120);
    }
    wire::order(account, ids.fresh(), fields)
}

/// Takes what the core said and keeps this book's picture of what rests in step with it.
fn absorb(book: &mut Book, results: &[Value], counts: &mut Counts) {
    for r in results {
        match r["msg"].as_str().unwrap_or("") {
            "OrderAccepted" => {
                counts.accepted += 1;
                book.resting.push((u(&r["orderId"]), u(&r["accountId"])));
            }
            "OrderRejected" => counts.rejected += 1,
            "OrderAmended" => counts.amended += 1,
            "OrderCancelled" => {
                counts.cancelled += 1;
                let gone = u(&r["orderId"]);
                book.resting.retain(|(order, _)| *order != gone);
            }
            "Fill" => {
                counts.fills += 1;
                let maker = (u(&r["makerOrderId"]), u(&r["makerRemaining"]));
                let taker = (u(&r["takerOrderId"]), u(&r["takerRemaining"]));
                for (order, remaining) in [maker, taker] {
                    if remaining == 0 {
                        book.resting.retain(|(resting, _)| *resting != order);
                    }
                }
            }
            _ => {}
        }
    }
}

pub fn run(ingress: &Submit, c: &Config) -> Result<(), Error> {
    let mut ids = Ids::new('k')?;
    let mut rng = Rng::new(c.seed);
    let mut counts = Counts::default();

    let began = now();
    let mut books = setup(ingress, &mut ids, c, &mut counts)?;
    println!("listed {} books and funded {} accounts in {}ms", c.books, c.accounts, began.elapsed().as_millis());

    // The markets each customer follows, drawn once from the same distribution the flow follows, so the crowd is
    // spread over the hot books the way it would be in life.
    let customers = c.accounts.saturating_sub(c.makers).max(1);
    let favourites: Vec<Vec<usize>> =
        (0..customers).map(|_| (0..FAVOURITES).map(|_| pick(&mut rng, c)).collect()).collect();

    let flow = now();
    let mut reported = 0u64;
    // Inputs the node has been given and not yet answered, with the book each was for, oldest first. A node
    // answers a connection in commit order, so taking them from the front takes them in the order they were sent.
    //
    // One consequence worth knowing when reading the counts: with several in flight the next action is chosen
    // from a book that is a little out of date, so the generator sometimes amends or cancels an order an earlier
    // request has already removed, and those come back rejected. Real participants race in exactly this way.
    let mut in_flight: VecDeque<(usize, Pending)> = VecDeque::new();
    while flow.elapsed() < Duration::from_secs(c.seconds) {
        // Most of what arrives is a market maker's; the crowd is many accounts each doing very little.
        let maker = rng.chance(700);
        let account = if maker { 1 + rng.below(c.makers.max(1)) } else { c.makers + 1 + rng.below(customers) };
        // A maker quotes wherever it likes; a customer only trades the markets it follows.
        let which = if maker {
            pick(&mut rng, c)
        } else {
            let who = ((account - c.makers - 1) % customers) as usize;
            favourites[who][rng.below(FAVOURITES) as usize]
        };
        let choice = rng.below(1000);
        let book = &mut books[which];
        // Per mille, because mass cancels are rare: they come from a session dropping or missing its heartbeats,
        // not from anybody's trading. Counted in whole percent they drown out everything else in the profile.
        let input = if choice < 600 || book.resting.is_empty() {
            new_order(&mut rng, &mut ids, book, account, maker)
        } else if choice < 900 {
            let (order, owner) = book.resting[rng.below(book.resting.len() as u64) as usize];
            let tick = (book.fair + rng.below(5)).saturating_sub(2).min(97);
            wire::amend(owner, book.instrument, order, tick, 1 + rng.below(20))
        } else if choice < 997 {
            let (order, owner) = book.resting[rng.below(book.resting.len() as u64) as usize];
            wire::cancel(owner, book.instrument, order)
        } else {
            // Scoped to this instrument, so everything it cancels belongs to the book being tracked.
            let owner = book.resting[rng.below(book.resting.len() as u64) as usize].1;
            json!({
                "msg": "MassCancel", "accountId": owner, "sessionId": owner * 10, "scope": "INSTRUMENT",
                "scopeRef": book.instrument, "cause": "CLIENT_REQUEST"
            })
        };
        // Given to the node without waiting for it. The book it was for travels with it, because its results only
        // mean anything against that book.
        counts.sent += 1;
        in_flight.push_back((which, ingress.submit(input)?));
        while in_flight.len() >= c.depth.max(1) {
            take_oldest(&mut in_flight, &mut books, &mut counts)?;
        }

        if counts.sent - reported >= 100_000 {
            reported = counts.sent;
            println!("  {} inputs, {} fills, {}/s", counts.sent, counts.fills, rate(counts.sent, &flow));
        }
    }
    // Whatever is still outstanding when the clock runs out still counts.
    while !in_flight.is_empty() {
        take_oldest(&mut in_flight, &mut books, &mut counts)?;
    }

    let resting: usize = books.iter().map(|b| b.resting.len()).sum();
    println!(
        "{} inputs in {}s: {}/s\n  accepted {}, amended {}, cancelled {}, rejected {}, fills {}\n  \
         {resting} orders left resting across {} books",
        counts.sent,
        flow.elapsed().as_secs(),
        rate(counts.sent, &flow),
        counts.accepted,
        counts.amended,
        counts.cancelled,
        counts.rejected,
        counts.fills,
        c.books
    );
    Ok(())
}

/// Waits for the oldest outstanding input and puts its results into the book it was for.
fn take_oldest(
    in_flight: &mut VecDeque<(usize, Pending)>,
    books: &mut [Book],
    counts: &mut Counts,
) -> Result<(), Error> {
    let Some((which, pending)) = in_flight.pop_front() else { return Ok(()) };
    let results = pending.wait()?;
    if let Some(book) = books.get_mut(which) {
        absorb(book, &results, counts);
    }
    Ok(())
}

/// Inputs a second, in whole numbers: nothing in this workspace is a float (blueprint §4).
fn rate(inputs: u64, since: &Instant) -> u64 {
    let millis = since.elapsed().as_millis().max(1);
    u64::try_from(u128::from(inputs) * 1000 / millis).unwrap_or(u64::MAX)
}
