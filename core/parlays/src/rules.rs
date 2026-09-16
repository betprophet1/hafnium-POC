//! What a parlay is, as pure functions of the feed's view: which leg sets may be requested, the instrument a leg set
//! names, the inputs that list it, and what the parlay service does as its legs trade, stop and resolve. Nothing here
//! talks to a node, so all of it is tested on views built by hand.
//!
//! A parlay pays $1 if every leg resolves as given (blueprint §6). It is listed under an event of its own with two
//! states, `Loses` (0) and `Wins` (1), and its YES pays in `Wins` (spec D-22), so `ExpireParlay` resolving it to the
//! lowest state outside its mask (E-15.7) is exactly "it lost".

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use venue_link::view::{Leg, View};
use venue_link::wire::command;

/// The parlay ladder's tick table. The demo catalogue's cent table is 1; nothing else in a mini venue defines tables.
pub const TICK_TABLE: u64 = 2;

/// Contracts per lot. The ladder's 0.1¢ rows are thousandths, and E-6.3 lists a book only if every row prices a lot
/// in whole cents, `100 · lot · n / 1000`: ten contracts is the least that does. It is also the smallest request, so
/// the minimum size the blueprint asks of an RFQ comes with the ladder.
pub const LOT_STEP: u64 = 10;

pub const MIN_LEGS: usize = 2;
pub const MAX_LEGS: usize = 6;

/// A parlay's YES pays in state 1.
pub const MASK: u64 = 0b10;
pub const LOSES: u64 = 0;
pub const WINS: u64 = 1;
pub const OUTCOMES: [&str; 2] = ["Loses", "Wins"];

/// The bottom of the ID range parlays are hashed into, for their events and instruments alike (the engine keeps the
/// two apart, so a parlay's event and instrument share one number). The demo catalogue's IDs grow from the bottom:
/// event `template × 1 000 000 + round` and instrument `event × 100 + index` stay below 2^52 for every template
/// below 45 million, which no catalogue approaches. The range stops at 2^53, so every ID is exact in a JSON number
/// as well as in the decimal strings of interface 1.
pub const ID_BASE: u64 = 1 << 52;
const ID_BITS: u64 = ID_BASE - 1;

/// Whether an event or instrument ID is in the parlay range.
pub const fn is_parlay_id(id: u64) -> bool {
    id >= ID_BASE && id < 2 * ID_BASE
}

/// The numerators of the parlay ladder, over 1000: 0.1¢ steps up to 10¢ and from 90¢, whole cents between. Parlays
/// live deep out of the money, where a cent is too coarse (a five-leg parlay may be worth 0.8¢), and a whole 0.1¢
/// ladder would cost 999 rows a side; each book reserves two levels per row whether it uses them or not (E-6.3), and
/// 279 rows let a small node list a few hundred parlays instead of a few dozen. The rows are symmetric about 500, so
/// the ladder is closed under complement (E-6.1).
pub fn ladder() -> Vec<u64> {
    (1..=100).chain((110..=890).step_by(10)).chain(900..=999).collect()
}

/// `DefineTickTable` for the parlay ladder, under a fixed command ID.
pub fn tick_table() -> Value {
    let numerators = ladder();
    let last = numerators.len() as u64 - 1;
    let ticks: Vec<Value> = numerators
        .iter()
        .enumerate()
        .map(|(t, n)| json!({"tick": t, "numerator": n, "denominator": 1000, "complement": last - t as u64}))
        .collect();
    command("DefineTickTable", "parlay-ticks".into(), json!({"tickTableId": TICK_TABLE, "ticks": ticks}))
}

/// Why a leg set cannot be requested: refused at the edge, and nothing is listed.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Something about the request itself.
    Bad(String),
    /// A leg names a market that does not exist.
    Unknown(u64),
}

/// A leg as a request names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wanted {
    pub instrument: u64,
    pub yes: bool,
}

/// The legs in canonical order, ascending by event, once every one is on an open, unresolved, non-parlay market and
/// no two share an event. Two legs on one event are refused rather than merged: they are either the same leg twice
/// or dependent in a way a product of prices would get wrong, and a responder should not be asked to untangle it.
pub fn canonical(view: &View, wanted: &[Wanted]) -> Result<Vec<Leg>, Refusal> {
    if !(MIN_LEGS..=MAX_LEGS).contains(&wanted.len()) {
        return Err(Refusal::Bad(format!("a parlay has {MIN_LEGS} to {MAX_LEGS} legs")));
    }
    let mut legs = Vec::with_capacity(wanted.len());
    for w in wanted {
        let market = view.instruments.get(&w.instrument).ok_or(Refusal::Unknown(w.instrument))?;
        if market.class == "PARLAY" {
            return Err(Refusal::Bad(format!("market {} is a parlay, and a parlay cannot be a leg", w.instrument)));
        }
        let resolved = view.events.get(&market.event).is_none_or(|e| e.resolved || e.resolution.is_some());
        if market.state != "OPEN" || resolved {
            return Err(Refusal::Bad(format!("market {} is not open", w.instrument)));
        }
        legs.push(Leg { event: market.event, instrument: w.instrument, yes: w.yes });
    }
    legs.sort_unstable_by_key(|l| (l.event, l.instrument, l.yes));
    if let Some(pair) = legs.windows(2).find(|pair| pair[0].event == pair[1].event) {
        return Err(Refusal::Bad(format!(
            "two legs are on event {}: every leg must be on its own event",
            pair[0].event
        )));
    }
    Ok(legs)
}

/// The parlay a canonical leg set names: 52 bits of a SHA-256 of the legs, above `ID_BASE`. Requests in any
/// order name the same instrument, because the legs are sorted first. Two different leg sets sharing an ID would take
/// tens of millions of parlays to become likely; the service refuses the second rather than mixing them up.
pub fn parlay_id(legs: &[Leg]) -> u64 {
    let mut text = String::from("parlay-v1");
    for leg in legs {
        text.push_str(&format!(";{}:{}:{}", leg.event, leg.instrument, if leg.yes { "YES" } else { "NO" }));
    }
    let digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    ID_BASE | (u64::from_be_bytes(head) & ID_BITS)
}

/// A command ID naming one thing done once to one parlay: a letter for the kind, then the ID in base 36. Resending
/// it is answered DUPLICATE (E-7.1), so a service that restarts, or two requests racing to list one parlay, do it
/// once. At most 12 bytes, inside the journal's 16.
pub fn command_id(kind: char, id: u64) -> String {
    let mut digits = Vec::new();
    let mut n = id;
    loop {
        digits.push(char::from_digit((n % 36) as u32, 36).unwrap_or('0'));
        n /= 36;
        if n == 0 {
            break;
        }
    }
    std::iter::once(kind).chain(digits.into_iter().rev()).collect()
}

/// The inputs that list a parlay and open it: its event, the instrument with its legs, and `OPEN`.
pub fn listing(legs: &[Leg]) -> Vec<Value> {
    let id = parlay_id(legs);
    let legs: Vec<Value> = legs
        .iter()
        .map(|l| {
            json!({"legEventId": l.event, "legInstrumentId": l.instrument, "legOutcome": if l.yes { "YES" } else { "NO" }})
        })
        .collect();
    vec![
        command("DefineEvent", command_id('e', id), json!({"eventId": id, "stateCount": 2, "scheduledStart": 0})),
        command(
            "ListInstrument",
            command_id('l', id),
            json!({
                "instrumentId": id, "eventId": id, "instrumentClass": "PARLAY", "stateMask": MASK,
                "tickTableId": TICK_TABLE, "lotStep": LOT_STEP, "allocationPolicy": "PRICE_TIME", "positionLimit": 0,
                "legs": legs
            }),
        ),
        command("SetMarketState", command_id('o', id), json!({"instrumentId": id, "state": "OPEN"})),
    ]
}

/// Where one leg stands, with the sequencer time it resolved at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegOutcome {
    Open,
    Won(u64),
    Lost(u64),
    Void(u64),
}

pub fn leg_outcome(view: &View, leg: &Leg) -> LegOutcome {
    let Some(r) = view.events.get(&leg.event).and_then(|e| e.resolution.as_ref()) else { return LegOutcome::Open };
    if r.kind == "VOID" {
        return LegOutcome::Void(r.at);
    }
    let mask = view.instruments.get(&leg.instrument).map_or(0, |i| i.mask);
    let yes_paid = r.winning_state < 64 && mask >> r.winning_state & 1 == 1;
    if yes_paid == leg.yes {
        LegOutcome::Won(r.at)
    } else {
        LegOutcome::Lost(r.at)
    }
}

/// How a parlay settles once its legs say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settlement {
    /// A leg lost: the parlay is worth nothing now, whatever the others do (E-15.7).
    Expire { causing_event: u64 },
    /// A leg was voided, and a voided leg voids the whole parlay.
    Void,
    /// Every leg won.
    Wins,
}

/// The settlement the legs call for, if any yet. The first leg to lose or be voided decides, by the time it resolved,
/// so a service that catches up on a journal where both have happened does what one watching live would have done;
/// a loss and a void at the same instant void, the outcome that takes nothing from anyone.
pub fn settlement(legs: &[(u64, LegOutcome)]) -> Option<Settlement> {
    let decisive = legs
        .iter()
        .filter_map(|&(event, outcome)| match outcome {
            LegOutcome::Void(at) => Some((at, 0, Settlement::Void)),
            LegOutcome::Lost(at) => Some((at, 1, Settlement::Expire { causing_event: event })),
            LegOutcome::Open | LegOutcome::Won(_) => None,
        })
        .min_by_key(|&(at, rank, _)| (at, rank));
    if let Some((_, _, settlement)) = decisive {
        return Some(settlement);
    }
    legs.iter().all(|(_, o)| matches!(o, LegOutcome::Won(_))).then_some(Settlement::Wins)
}

/// The market state a parlay should be in while it is unresolved: open while every leg trades, halted while one is
/// halted or its event suspended, and closed once any leg has stopped trading for good, closed or resolved. A
/// parlay on a leg nobody can trade any more is a bet on a result someone may already know.
pub fn trading_state(view: &View, legs: &[Leg]) -> &'static str {
    let mut halted = false;
    for leg in legs {
        let market = view.instruments.get(&leg.instrument).map_or("CLOSED", |i| i.state.as_str());
        let event = view.events.get(&leg.event);
        if event.is_none_or(|e| e.resolved || e.resolution.is_some()) {
            return "CLOSED";
        }
        match market {
            "OPEN" => halted |= event.is_some_and(|e| e.suspended),
            "HALTED" | "LISTED" => halted = true,
            _ => return "CLOSED",
        }
    }
    if halted {
        "HALTED"
    } else {
        "OPEN"
    }
}

/// Something the service should sequence for one parlay.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Settle(Settlement),
    /// A `SetMarketState` to this state.
    Move(&'static str),
}

/// What every unresolved parlay needs now, in ascending instrument order. Settling comes before moving: a parlay
/// that can settle need not close first, since resolution removes its orders itself (E-15.5).
pub fn actions(view: &View) -> Vec<(u64, Action)> {
    let mut out = Vec::new();
    for (&id, parlay) in view.instruments.iter().filter(|(_, i)| i.class == "PARLAY") {
        if view.events.get(&parlay.event).is_none_or(|e| e.resolved || e.resolution.is_some()) {
            continue;
        }
        let outcomes: Vec<(u64, LegOutcome)> = parlay.legs.iter().map(|l| (l.event, leg_outcome(view, l))).collect();
        if let Some(settlement) = settlement(&outcomes) {
            out.push((id, Action::Settle(settlement)));
            continue;
        }
        let wanted = trading_state(view, &parlay.legs);
        // E-16.1's transitions: a parlay closes from open or halted, and moves between those two.
        let allowed = matches!(
            (parlay.state.as_str(), wanted),
            ("OPEN", "HALTED") | ("HALTED", "OPEN") | ("OPEN" | "HALTED", "CLOSED")
        );
        if allowed {
            out.push((id, Action::Move(wanted)));
        }
    }
    out
}

/// The input for a settlement. Each has a command ID of its own kind for the parlay, so it is sequenced once.
pub fn settle_input(parlay: u64, settlement: Settlement) -> Value {
    match settlement {
        Settlement::Expire { causing_event } => command(
            "ExpireParlay",
            command_id('x', parlay),
            json!({"instrumentId": parlay, "causingEventId": causing_event}),
        ),
        Settlement::Void => command(
            "ResolveEvent",
            command_id('v', parlay),
            json!({"eventId": parlay, "kind": "VOID", "caseId": 0, "winningState": 0, "voidWeights": []}),
        ),
        Settlement::Wins => command(
            "ResolveEvent",
            command_id('w', parlay),
            json!({"eventId": parlay, "kind": "RESOLVED", "caseId": 0, "winningState": WINS, "voidWeights": []}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use venue_link::wire::Unit;

    /// Applies one input to a view as the feed would deliver it: applied, at `at`.
    fn apply(view: &mut View, msg: Value, at: u64) {
        let mut input = msg;
        input["record"] = json!({"sequence": at, "sequencedAt": at, "sequencerGroup": 1});
        let results = vec![json!({"msg": "CommandApplied", "outcome": "APPLIED"})];
        view.apply(&Unit { input, results });
    }

    fn state(view: &mut View, instrument: u64, state: &str, at: u64) {
        let mut unit_input = json!({"msg": "SetMarketState", "instrumentId": instrument, "state": state});
        unit_input["record"] = json!({"sequence": at, "sequencedAt": at, "sequencerGroup": 1});
        let results = vec![
            json!({"msg": "CommandApplied", "outcome": "APPLIED"}),
            json!({"msg": "MarketStateChanged", "instrumentId": instrument, "state": state}),
        ];
        view.apply(&Unit { input: unit_input, results });
    }

    /// Three open binaries on events 1, 2 and 3 (instruments 100, 200, 300, YES paying in state 1), and a
    /// three-outcome event 4 with one market per outcome (400, 401, 402).
    fn venue() -> View {
        let mut view = View::default();
        let mut at = 1;
        for event in 1..=3u64 {
            apply(&mut view, json!({"msg": "DefineEvent", "eventId": event, "stateCount": 2, "scheduledStart": 0}), at);
            let list = json!({"msg": "ListInstrument", "instrumentId": event * 100, "eventId": event,
                              "instrumentClass": "BINARY", "stateMask": 2, "tickTableId": 1, "lotStep": 1, "legs": []});
            apply(&mut view, list, at + 1);
            state(&mut view, event * 100, "OPEN", at + 2);
            at += 3;
        }
        apply(&mut view, json!({"msg": "DefineEvent", "eventId": 4, "stateCount": 3, "scheduledStart": 0}), at);
        for k in 0..3u64 {
            let list = json!({"msg": "ListInstrument", "instrumentId": 400 + k, "eventId": 4,
                              "instrumentClass": "CATEGORICAL_OUTCOME", "stateMask": 1u64 << k, "tickTableId": 1,
                              "lotStep": 1, "legs": []});
            apply(&mut view, list, at + 1 + k);
            state(&mut view, 400 + k, "OPEN", at + 4 + k);
        }
        view
    }

    fn wanted(instrument: u64, yes: bool) -> Wanted {
        Wanted { instrument, yes }
    }

    fn resolve(view: &mut View, event: u64, kind: &str, winning: u64, at: u64) {
        let msg = json!({"msg": "ResolveEvent", "eventId": event, "kind": kind, "caseId": 0, "winningState": winning});
        apply(view, msg, at);
    }

    /// Lists the parlay on `legs` into the view, as the node would once the service's listing applies.
    fn list(view: &mut View, legs: &[Leg], at: u64) -> u64 {
        for (k, input) in listing(legs).into_iter().enumerate() {
            if input["msg"] == "SetMarketState" {
                state(view, parlay_id(legs), "OPEN", at + k as u64);
            } else {
                apply(view, input, at + k as u64);
            }
        }
        parlay_id(legs)
    }

    #[test]
    fn legs_are_ordered_by_event_whatever_order_they_were_asked_in() {
        let view = venue();
        let a = canonical(&view, &[wanted(300, true), wanted(100, false), wanted(401, true)]).expect("valid");
        let b = canonical(&view, &[wanted(401, true), wanted(300, true), wanted(100, false)]).expect("valid");
        assert_eq!(a, b);
        assert_eq!(a.iter().map(|l| l.event).collect::<Vec<_>>(), vec![1, 3, 4]);
        assert_eq!(parlay_id(&a), parlay_id(&b));
    }

    #[test]
    fn a_leg_set_is_refused_unless_every_leg_is_open_on_an_event_of_its_own() {
        let mut view = venue();
        let refused = |view: &View, legs: &[Wanted]| canonical(view, legs).expect_err("refused");
        assert!(matches!(refused(&view, &[wanted(100, true)]), Refusal::Bad(_)), "one leg");
        let seven: Vec<Wanted> = [100, 200, 300, 400, 100, 200, 300].iter().map(|&i| wanted(i, true)).collect();
        assert!(matches!(refused(&view, &seven), Refusal::Bad(_)), "seven legs");
        assert!(matches!(refused(&view, &[wanted(400, true), wanted(402, false)]), Refusal::Bad(_)), "one event");
        assert!(matches!(refused(&view, &[wanted(100, true), wanted(100, false)]), Refusal::Bad(_)), "one market");
        assert_eq!(refused(&view, &[wanted(100, true), wanted(999, true)]), Refusal::Unknown(999));
        state(&mut view, 200, "CLOSED", 100);
        assert!(matches!(refused(&view, &[wanted(100, true), wanted(200, true)]), Refusal::Bad(_)), "closed leg");
        let legs = canonical(&view, &[wanted(100, true), wanted(300, true)]).expect("valid");
        let parlay = list(&mut view, &legs, 200);
        assert!(matches!(refused(&view, &[wanted(parlay, true), wanted(400, true)]), Refusal::Bad(_)), "parlay leg");
    }

    #[test]
    fn a_parlay_id_is_in_its_own_range_and_depends_on_every_leg() {
        let view = venue();
        let legs = canonical(&view, &[wanted(100, true), wanted(200, true)]).expect("valid");
        let id = parlay_id(&legs);
        assert!(is_parlay_id(id) && id < 1 << 53, "{id}");
        let catalogue_event = venue_link::catalogue::event_id(45_000_000, 999_999);
        let catalogue_market = venue_link::catalogue::instrument_id(catalogue_event, 99);
        assert!(!is_parlay_id(catalogue_event) && !is_parlay_id(catalogue_market), "{catalogue_market}");
        let other_outcome = canonical(&view, &[wanted(100, true), wanted(200, false)]).expect("valid");
        let other_market = canonical(&view, &[wanted(100, true), wanted(300, true)]).expect("valid");
        assert_ne!(id, parlay_id(&other_outcome));
        assert_ne!(id, parlay_id(&other_market));
        assert_eq!(id, parlay_id(&legs), "the same legs always name the same parlay");
    }

    #[test]
    fn command_ids_fit_the_journal_and_differ_by_kind() {
        let longest = command_id('x', 2 * ID_BASE - 1);
        assert!(longest.len() <= 16, "{longest}");
        assert_ne!(command_id('x', ID_BASE + 5), command_id('w', ID_BASE + 5));
        assert_eq!(command_id('e', 36), "e10");
    }

    #[test]
    fn the_ladder_is_closed_under_complement_and_prices_a_lot_in_whole_cents() {
        let numerators = ladder();
        assert_eq!(numerators.len(), 279);
        assert!(numerators.windows(2).all(|w| w[0] < w[1]));
        let table = tick_table();
        let rows = table["ticks"].as_array().expect("rows");
        for row in rows {
            let (n, c) = (row["numerator"].as_u64().unwrap_or(0), row["complement"].as_u64().unwrap_or(0) as usize);
            assert_eq!(n + rows[c]["numerator"].as_u64().unwrap_or(0), 1000, "{row}");
            assert_eq!(100 * LOT_STEP * n % 1000, 0, "{row}");
        }
    }

    #[test]
    fn a_lost_leg_expires_the_parlay_at_once() {
        let mut view = venue();
        let legs = canonical(&view, &[wanted(100, true), wanted(200, false), wanted(400, true)]).expect("valid");
        let parlay = list(&mut view, &legs, 100);
        assert_eq!(actions(&view), vec![]);
        // Event 2 resolves to state 1, where YES pays: the leg wanted NO.
        resolve(&mut view, 2, "RESOLVED", 1, 200);
        assert_eq!(actions(&view), vec![(parlay, Action::Settle(Settlement::Expire { causing_event: 2 }))]);
        assert_eq!(settle_input(parlay, Settlement::Expire { causing_event: 2 })["msg"], "ExpireParlay");
    }

    #[test]
    fn every_leg_winning_resolves_the_parlay_to_wins() {
        let mut view = venue();
        let legs = canonical(&view, &[wanted(100, true), wanted(401, false)]).expect("valid");
        let parlay = list(&mut view, &legs, 100);
        resolve(&mut view, 1, "RESOLVED", 1, 200);
        // One leg won and the other has not resolved: the parlay stops trading and waits.
        assert_eq!(actions(&view), vec![(parlay, Action::Move("CLOSED"))]);
        state(&mut view, parlay, "CLOSED", 201);
        assert_eq!(actions(&view), vec![]);
        // State 2 is Chelsea; the leg wanted Draw (401) not to happen.
        resolve(&mut view, 4, "RESOLVED", 2, 300);
        assert_eq!(actions(&view), vec![(parlay, Action::Settle(Settlement::Wins))]);
        assert_eq!(settle_input(parlay, Settlement::Wins)["winningState"], WINS);
    }

    #[test]
    fn a_voided_leg_voids_the_parlay_even_when_the_others_won() {
        let mut view = venue();
        let legs = canonical(&view, &[wanted(100, true), wanted(200, true)]).expect("valid");
        let parlay = list(&mut view, &legs, 100);
        resolve(&mut view, 1, "RESOLVED", 1, 200);
        resolve(&mut view, 2, "VOID", 0, 300);
        assert_eq!(actions(&view), vec![(parlay, Action::Settle(Settlement::Void))]);
    }

    #[test]
    fn whichever_of_a_loss_and_a_void_came_first_decides() {
        let at = |outcomes: &[LegOutcome]| {
            settlement(&outcomes.iter().enumerate().map(|(k, o)| (k as u64 + 1, *o)).collect::<Vec<_>>())
        };
        assert_eq!(at(&[LegOutcome::Lost(5), LegOutcome::Void(9)]), Some(Settlement::Expire { causing_event: 1 }));
        assert_eq!(at(&[LegOutcome::Lost(9), LegOutcome::Void(5)]), Some(Settlement::Void));
        assert_eq!(at(&[LegOutcome::Lost(5), LegOutcome::Void(5)]), Some(Settlement::Void));
        assert_eq!(at(&[LegOutcome::Won(1), LegOutcome::Open]), None);
        assert_eq!(at(&[LegOutcome::Won(1), LegOutcome::Won(2)]), Some(Settlement::Wins));
    }

    #[test]
    fn a_parlay_follows_its_legs_between_open_halted_and_closed() {
        let mut view = venue();
        let legs = canonical(&view, &[wanted(100, true), wanted(300, true)]).expect("valid");
        let parlay = list(&mut view, &legs, 100);
        state(&mut view, 300, "HALTED", 200);
        assert_eq!(actions(&view), vec![(parlay, Action::Move("HALTED"))]);
        state(&mut view, parlay, "HALTED", 201);
        state(&mut view, 300, "OPEN", 202);
        assert_eq!(actions(&view), vec![(parlay, Action::Move("OPEN"))]);
        state(&mut view, parlay, "OPEN", 203);
        apply(&mut view, json!({"msg": "SetEventPhase", "eventId": 1, "phase": "SUSPENDED"}), 204);
        assert_eq!(actions(&view), vec![(parlay, Action::Move("HALTED"))]);
        // A leg that closes before anyone traded the parlay closes it for good; it settles with its legs later.
        state(&mut view, 300, "CLOSED", 205);
        assert_eq!(actions(&view), vec![(parlay, Action::Move("CLOSED"))]);
        state(&mut view, parlay, "CLOSED", 206);
        apply(&mut view, json!({"msg": "SetEventPhase", "eventId": 1, "phase": "RESUMED"}), 207);
        assert_eq!(actions(&view), vec![]);
        resolve(&mut view, 3, "RESOLVED", 0, 300);
        assert_eq!(actions(&view), vec![(parlay, Action::Settle(Settlement::Expire { causing_event: 3 }))]);
    }

    #[test]
    fn a_resolved_parlay_needs_nothing_more() {
        let mut view = venue();
        let legs = canonical(&view, &[wanted(100, true), wanted(200, true)]).expect("valid");
        let parlay = list(&mut view, &legs, 100);
        resolve(&mut view, 1, "RESOLVED", 0, 200);
        resolve(&mut view, parlay, "RESOLVED", LOSES, 201);
        resolve(&mut view, 2, "VOID", 0, 300);
        assert_eq!(actions(&view), vec![]);
    }
}
