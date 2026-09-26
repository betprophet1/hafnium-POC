//! `venue-sim oracle`: the dummy oracle of blueprint §16, run on a calendar, with the subset of its control API
//! (interface 23) the mini venue needs.
//!
//! It is an operator tool, like `seed`: it follows the core's feed and sends commands to its ingress. Every pass it
//! looks at the venue as the feed shows it and at the sequencer's time, and does whatever the calendar says is due:
//! it lists the next round of a template (`DefineEvent` with its scheduled start, the markets, `OPEN`), starts,
//! suspends, resumes and ends events (`SetEventPhase` with source `DUMMY_ORACLE`), closes markets, and opens a
//! resolution case once they have closed. Cases (`resolution.rs`) collect reports from the two dummy sources and the
//! referee, propose, sign, wait out the dispute window and sequence `ResolveEvent`. What happens by hand through the
//! API is sequenced with source `OPERATOR`, so the journal says who decided each phase.
//!
//! State: the calendar's decisions are derived from the journal, so a restarted oracle carries on where the venue
//! is. Resolution cases (reports, proposals, sign-offs, disputes, history), postponements and suspensions by hand are
//! written to PostgreSQL (`cases.rs`) whenever they change, before anything is done about the change, and read back at
//! start, so a restarted oracle carries each case on where it was. While the database cannot be reached, cases do not
//! move and manual actions on them are refused; the calendar carries on. Without a database (`--store memory`) they
//! live only in this process, and a case is opened afresh after a restart.
//!
//! Time: every timer runs on the sequencer's clock, which the oracle reads from the feed. The node leaves ticks that
//! change nothing off its feed, so while the venue is quiet the last time the feed carried stands still; the oracle
//! then counts on from it by its own monotonic clock. Everything it records comes from the journal regardless.
//!
//! Capacity: the engine releases a settled event and its markets a retention after resolution (E-8.4; the seed sets
//! half an hour), and refuses a listing past the seed's limits with `LISTING_CAPACITY` (E-6.2, E-6.3), so the venue
//! runs indefinitely. The oracle is still told the node's profile and holds a new round while it would take the venue
//! past 95% of its events, markets or price levels: a round is several commands, and a refusal part-way through would
//! leave an event with only some of its markets. It says so on the schedule, and carries on once releases make room.
//!
//! The control API, all JSON, times in milliseconds since the epoch on the sequencer's clock:
//!
//! | Request | Does |
//! |---|---|
//! | `GET /oracle/schedule` | templates with their next round; open and upcoming events and a few recent ones, each with its timings and case |
//! | `POST /oracle/templates/{template}/list` | lists the template's next round now |
//! | `POST /oracle/events/{event}/start`, `suspend`, `resume`, `end`, `close` | the phase or market change, now |
//! | `POST /oracle/events/{event}/propose` `{"winningState": n, "evidence": "..."}` | an operator's proposal |
//! | `POST /oracle/events/{event}/void` `{"evidence": "..."}` | a proposal to void |
//! | `POST /oracle/events/{event}/dispute` `{"reason": "..."}` | disputes the proposal in its window |
//! | `POST /oracle/events/{event}/postpone` `{"seconds": n}` | holds a round that has not started |

use crate::calendar::{Calendar, Facts, Step, Timing, NANOS};
use crate::cases::CaseStore;
use crate::resolution::{Action, Case, Outcome, Source, Stage};
use crate::session::monotonic;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use venue_link::catalogue::{self, split_event, Catalogue};
use venue_link::view::{Live, View};
use venue_link::wire::{self, Error, Ids, Ingress};

/// How often the oracle looks for something due. Stages are seconds apart; a quarter of one is prompt enough.
const PASS: Duration = Duration::from_millis(250);
/// Settled events the schedule shows, newest first, and settled cases kept for it.
const RECENT: usize = 6;
/// How long a sent `ResolveEvent` may go unseen on the feed before it is sent again (with the same command ID).
const RESEND: u64 = 5 * NANOS;
/// Share of each engine capacity, in percent, beyond which no new round is listed.
const CAPACITY_PERCENT: usize = 95;

/// The engine capacities a listing uses up, from the node's capacity profile.
pub struct Capacity {
    pub events: usize,
    pub instruments: usize,
    pub levels: usize,
}

pub struct Oracle {
    calendar: Calendar,
    catalogue: Catalogue,
    capacity: Capacity,
    live: Arc<Live>,
    ingress: Mutex<(Ingress, Ids)>,
    desk: Mutex<Desk>,
    /// The last sequencer time the feed carried, and when this process saw it: see the module comment, on time.
    clock: Mutex<(u64, Instant)>,
    /// Where cases and manual actions are kept; `None`: memory only.
    store: Option<CaseStore>,
}

/// What the oracle holds that the journal does not.
#[derive(Default)]
struct Desk {
    cases: BTreeMap<u64, Case>,
    /// Postponed rounds, until when.
    holds: BTreeMap<u64, u64>,
    suspended_by_hand: BTreeSet<u64>,
    /// When a case's `ResolveEvent` was last sent.
    sent: BTreeMap<u64, u64>,
    /// Why listing is paused, if it is.
    paused: Option<String>,
    /// Why cases are paused, if they are: the records could not be written.
    cases_paused: Option<String>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wall-clock nanoseconds, only to judge whether the feed has caught up with the sequencer.
#[allow(clippy::disallowed_methods)]
fn wall_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// A refusal the API answers with a status.
struct Refusal(u16, String);

impl From<Error> for Refusal {
    fn from(e: Error) -> Self {
        Self(400, e.to_string())
    }
}

fn refuse<T>(status: u16, message: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal(status, message.into()))
}

/// Commands to send, with what they are for, for the log.
type Batch = Vec<(String, Vec<Value>)>;

impl Oracle {
    pub fn new(
        calendar: Calendar,
        catalogue: Catalogue,
        capacity: Capacity,
        live: Arc<Live>,
        ingress: Ingress,
        store: Option<CaseStore>,
    ) -> Result<Self, Error> {
        for template in calendar.templates.keys() {
            if !catalogue.templates().contains(template) {
                return Err(format!("the calendar names template {template}, which the catalogue does not list").into());
            }
        }
        let ids = Ids::new('o')?;
        let clock = Mutex::new((0, monotonic()));
        let mut desk = Desk::default();
        if let Some(store) = &store {
            let held = store.load(RECENT * 4).map_err(|e| format!("reading the oracle's cases: {e}"))?;
            for case in held.cases {
                // A case that was sequencing when the oracle stopped sends its ResolveEvent again at once, under the
                // same command ID, which the sequencer group deduplicates.
                if matches!(case.stage, Stage::Sequencing(_)) {
                    desk.sent.insert(case.event, 0);
                }
                desk.cases.insert(case.event, case);
            }
            desk.holds = held.holds;
            desk.suspended_by_hand = held.suspended_by_hand;
            eprintln!(
                "oracle: {} cases, {} postponements and {} suspensions by hand read from the database",
                desk.cases.len(),
                desk.holds.len(),
                desk.suspended_by_hand.len()
            );
        }
        Ok(Self {
            calendar,
            catalogue,
            capacity,
            live,
            ingress: Mutex::new((ingress, ids)),
            desk: Mutex::new(desk),
            clock,
            store,
        })
    }

    /// The sequencer's time now: the feed's latest, counted on by this process's clock for as long as the feed has
    /// had nothing to say. 0 until the feed has said anything.
    fn now(&self, view: &View) -> u64 {
        let mut clock = locked(&self.clock);
        if view.now != clock.0 {
            *clock = (view.now, monotonic());
        }
        if clock.0 == 0 {
            return 0;
        }
        let quiet = u64::try_from(monotonic().duration_since(clock.1).as_nanos()).unwrap_or(u64::MAX);
        clock.0.saturating_add(quiet)
    }

    fn timing(&self, event: u64) -> Option<&Timing> {
        self.calendar.templates.get(&split_event(event).0)
    }

    fn names(&self, event: u64, view: &View) -> Vec<String> {
        self.catalogue.states(event, view.events.get(&event).map_or(0, |e| e.state_count))
    }

    fn facts(view: &View, desk: &Desk, event: u64) -> Option<Facts> {
        let e = view.events.get(&event)?;
        let tradable = e
            .instruments
            .iter()
            .copied()
            .filter(|i| view.instruments.get(i).is_some_and(|i| matches!(i.state.as_str(), "OPEN" | "HALTED")))
            .collect();
        Some(Facts {
            defined_at: e.defined_at,
            scheduled_start: e.scheduled_start,
            started_at: e.started_at,
            suspended: e.suspended,
            suspended_ever: e.suspended_ever,
            suspended_by_hand: desk.suspended_by_hand.contains(&event),
            ended_at: e.ended_at,
            tradable,
            resolved: e.resolved,
        })
    }

    /// Why the next round of `template` cannot be listed without risking the engine's capacity, if it cannot.
    fn no_room(&self, view: &View, template: u64) -> Option<String> {
        let markets = self.catalogue.market_count(template);
        let rows = view.tables.get(&catalogue::TICK_TABLE).map_or(0, Vec::len);
        let levels_used: usize =
            view.instruments.values().map(|i| 2 * view.tables.get(&i.table).map_or(0, Vec::len)).sum();
        let within = |used: usize, adding: usize, limit: usize| (used + adding) * 100 <= limit * CAPACITY_PERCENT;
        let checks = [
            ("events", view.events.len(), 1, self.capacity.events),
            ("markets", view.instruments.len(), markets, self.capacity.instruments),
            ("price levels", levels_used, markets * 2 * rows, self.capacity.levels),
        ];
        checks.iter().find(|(_, used, adding, limit)| !within(*used, *adding, *limit)).map(|(what, used, _, limit)| {
            format!("listing paused: the venue holds {used} of its {limit} {what} until settled rounds are released")
        })
    }

    /// Whether `event` is a parlay's. The parlay service lists parlays on request and settles them from their legs
    /// (`core/parlays`): they are no template's rounds, and the oracle neither schedules, shows nor resolves them.
    fn is_parlay(view: &View, event: u64) -> bool {
        view.events
            .get(&event)
            .is_some_and(|e| e.instruments.iter().any(|i| view.instruments.get(i).is_some_and(|i| i.class == "PARLAY")))
    }

    /// The latest round of each template and when it was listed, released rounds included (E-8.4), so a template
    /// whose rounds have all been released counts on from the last rather than listing round 1 again.
    fn latest_rounds(view: &View) -> BTreeMap<u64, (u64, u64)> {
        let mut latest = view.released_rounds.clone();
        for (&id, e) in view.events.iter().filter(|(&id, _)| !Self::is_parlay(view, id)) {
            let (template, round) = split_event(id);
            let entry = latest.entry(template).or_insert((round, e.defined_at));
            if round >= entry.0 {
                *entry = (round, e.defined_at);
            }
        }
        latest
    }

    fn phase(ids: &mut Ids, event: u64, phase: &str, source: &str) -> Value {
        wire::command("SetEventPhase", ids.fresh(), json!({"eventId": event, "phase": phase, "source": source}))
    }

    fn close(ids: &mut Ids, markets: &[u64]) -> Vec<Value> {
        markets
            .iter()
            .map(|i| wire::command("SetMarketState", ids.fresh(), json!({"instrumentId": i, "state": "CLOSED"})))
            .collect()
    }

    fn resolve(event: u64, outcome: Outcome, revision: u64) -> Value {
        let (kind, state) = match outcome {
            Outcome::State(s) => ("RESOLVED", s),
            Outcome::Void => ("VOID", 0),
        };
        wire::command(
            "ResolveEvent",
            format!("rs{event}-{revision}"),
            json!({
                "eventId": event, "kind": kind, "caseId": Case::case_id(event, revision), "winningState": state,
                "voidWeights": []
            }),
        )
    }

    /// Everything due now, decided under the locks and sent after them.
    fn plan(&self) -> Batch {
        let view = locked(&self.live.view);
        let now = self.now(&view);
        // Before the feed has caught up, the view is the venue as it was some time ago, and acting on it would
        // repeat what was done back then.
        if now == 0 || wall_ns().saturating_sub(now) > 3 * NANOS {
            return Vec::new();
        }
        let mut desk = locked(&self.desk);
        let before = (desk.cases.clone(), desk.sent.clone());
        let mut ids_guard = locked(&self.ingress);
        let ids = &mut ids_guard.1;
        let mut batch = Batch::new();
        let latest = Self::latest_rounds(&view);
        desk.paused = None;
        for (&template, timing) in &self.calendar.templates {
            let Some((round, start)) = Calendar::listing_due(timing, latest.get(&template).copied(), now) else {
                continue;
            };
            if let Some(reason) = self.no_room(&view, template) {
                desk.paused = Some(reason);
                continue;
            }
            if let Ok(inputs) = self.catalogue.listing(template, round, start) {
                batch.push((format!("list {}", self.catalogue.title(catalogue::event_id(template, round))), inputs));
            }
        }
        for (&event, e) in &view.events {
            if e.resolved || Self::is_parlay(&view, event) {
                continue;
            }
            let Some(facts) = Self::facts(&view, &desk, event) else { continue };
            // A case under way freezes the round: its markets close, and the calendar does nothing else to it.
            if desk.cases.contains_key(&event) {
                if !facts.tradable.is_empty() {
                    batch.push((format!("close {event} for resolution"), Self::close(ids, &facts.tradable)));
                }
                continue;
            }
            let Some(timing) = self.timing(event) else { continue };
            let hold = desk.holds.get(&event).copied().unwrap_or(0);
            let step = self.calendar.due(timing, event, &facts, now, hold);
            let source = "DUMMY_ORACLE";
            match step {
                Some(Step::Start) => {
                    batch.push((format!("start {event}"), vec![Self::phase(ids, event, "STARTED", source)]))
                }
                Some(Step::Suspend) => {
                    batch.push((format!("suspend {event}"), vec![Self::phase(ids, event, "SUSPENDED", source)]));
                }
                Some(Step::Resume) => {
                    batch.push((format!("resume {event}"), vec![Self::phase(ids, event, "RESUMED", source)]));
                }
                Some(Step::End) => batch.push((format!("end {event}"), vec![Self::phase(ids, event, "ENDED", source)])),
                Some(Step::Close) => batch.push((format!("close {event}"), Self::close(ids, &facts.tradable))),
                Some(Step::Report) => {
                    let names = self.names(event, &view);
                    let mut case = Case::open(event, e.state_count, now);
                    for source in [Source::Alpha, Source::Beta] {
                        let report = self.calendar.report(timing, event, &names, source, now);
                        case.report(report, &self.calendar.timers, &names);
                    }
                    desk.cases.insert(event, case);
                }
                None => {}
            }
        }
        self.tick_cases(&view, now, &mut desk, &mut batch);
        self.persist(&mut desk, before, &mut batch);
        batch
    }

    /// Writes every case this pass changed, before its commands are sent. A case that cannot be written is put back as
    /// it was and its `ResolveEvent` is not sent: cases are paused until the records answer again.
    fn persist(&self, desk: &mut Desk, before: (BTreeMap<u64, Case>, BTreeMap<u64, u64>), batch: &mut Batch) {
        let Some(store) = &self.store else { return };
        let (cases, sent) = before;
        let changed: Vec<u64> = desk
            .cases
            .iter()
            .filter(|(event, case)| cases.get(event) != Some(*case))
            .map(|(event, _)| *event)
            .collect();
        let mut failure = None;
        let quiet = changed.is_empty();
        for event in changed {
            let Some(case) = desk.cases.get(&event) else { continue };
            if let Err(e) = store.save(case) {
                match cases.get(&event) {
                    Some(old) => desk.cases.insert(event, old.clone()),
                    None => desk.cases.remove(&event),
                };
                match sent.get(&event) {
                    Some(at) => desk.sent.insert(event, *at),
                    None => desk.sent.remove(&event),
                };
                let resolve = format!("resolve {event}");
                batch.retain(|(what, _)| *what != resolve);
                failure = Some(e.to_string());
            }
        }
        match (failure, desk.cases_paused.is_some()) {
            (Some(e), false) => {
                eprintln!("oracle: cases paused, their records could not be written: {e}");
                desk.cases_paused = Some(format!("cases paused: the oracle's database is unavailable ({e})"));
            }
            // Nothing was written this pass: only an answering database lifts the pause.
            (None, true) if quiet && store.ping().is_err() => {}
            (None, true) => {
                eprintln!("oracle: cases carry on, their records can be written again");
                desk.cases_paused = None;
            }
            _ => {}
        }
    }

    /// Writes one case after a manual action, or refuses the action.
    fn save_case(&self, desk: &Desk, event: u64) -> Result<(), Refusal> {
        match (&self.store, desk.cases.get(&event)) {
            (Some(store), Some(case)) => store.save(case).map_err(|e| {
                eprintln!("oracle: case {event} could not be written: {e}");
                Refusal(503, format!("the oracle's database is unavailable, nothing was done: {e}"))
            }),
            _ => Ok(()),
        }
    }

    /// Writes a manual postponement or suspension, or refuses the action.
    fn save_manual(&self, write: impl FnOnce(&CaseStore) -> Result<(), venue_store::Error>) -> Result<(), Refusal> {
        match &self.store {
            Some(store) => write(store).map_err(|e| {
                eprintln!("oracle: a manual action could not be written: {e}");
                Refusal(503, format!("the oracle's database is unavailable, nothing was done: {e}"))
            }),
            None => Ok(()),
        }
    }

    fn tick_cases(&self, view: &View, now: u64, desk: &mut Desk, batch: &mut Batch) {
        let default = Timing::default();
        let mut sends = Vec::new();
        for (&event, case) in &mut desk.cases {
            let names = self.names(event, view);
            if let Some(r) = view.events.get(&event).and_then(|e| e.resolution.as_ref()) {
                let outcome = if r.kind == "VOID" { Outcome::Void } else { Outcome::State(r.winning_state) };
                case.settled(outcome, now, &names);
                continue;
            }
            match case.tick(now, &self.calendar.timers, &names) {
                Some(Action::AskReferee) => {
                    let timing = self.timing(event).unwrap_or(&default);
                    let report = self.calendar.report(timing, event, &names, Source::Referee, now);
                    case.report(report, &self.calendar.timers, &names);
                }
                Some(Action::Sequence(p)) => sends.push((event, p.outcome, p.revision)),
                None => {
                    if let Stage::Sequencing(p) = &case.stage {
                        if desk.sent.get(&event).is_some_and(|at| now >= at + RESEND) {
                            sends.push((event, p.outcome, p.revision));
                        }
                    }
                }
            }
        }
        for (event, outcome, revision) in sends {
            desk.sent.insert(event, now);
            batch.push((format!("resolve {event}"), vec![Self::resolve(event, outcome, revision)]));
        }
        // Keep the few settled cases the schedule shows, and nothing else of the past.
        let settled: Vec<u64> =
            desk.cases.iter().filter(|(_, c)| matches!(c.stage, Stage::Settled(_))).map(|(e, _)| *e).collect();
        for event in settled.iter().take(settled.len().saturating_sub(RECENT * 4)) {
            desk.cases.remove(event);
            desk.sent.remove(event);
            desk.holds.remove(event);
            desk.suspended_by_hand.remove(event);
        }
    }

    /// Sends a batch, logging what the engine refused, and returns the last input sequence it produced.
    fn send(&self, batch: Batch) -> Result<u64, Error> {
        let mut last = 0;
        for (what, inputs) in batch {
            for input in inputs {
                let results = locked(&self.ingress).0.send(input)?;
                for r in &results {
                    last = last.max(r["result"]["inputSequence"].as_u64().unwrap_or(0));
                    if r["msg"] == "CommandApplied" && r["outcome"] == "REJECTED" {
                        eprintln!("oracle: {what}: rejected {}", r["rejectReason"].as_str().unwrap_or("?"));
                    }
                }
            }
            eprintln!("oracle: {what}");
        }
        Ok(last)
    }

    /// Waits, up to a few seconds, for the feed to show an input, so the next pass decides from what it did.
    fn await_feed(&self, sequence: u64) {
        let deadline = monotonic() + Duration::from_secs(5);
        let mut view = locked(&self.live.view);
        while view.sequence < sequence && monotonic() < deadline {
            view = match self.live.changed.wait_timeout(view, Duration::from_millis(100)) {
                Ok((v, _)) => v,
                Err(p) => p.into_inner().0,
            };
        }
    }

    /// The calendar, for ever.
    pub fn run(&self) -> ! {
        loop {
            thread::sleep(PASS);
            let batch = self.plan();
            if batch.is_empty() {
                continue;
            }
            match self.send(batch) {
                Ok(last) => self.await_feed(last),
                Err(e) => {
                    eprintln!("oracle: the core's ingress: {e}");
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }

    // ------------------------------------------------------------------------------------------ control API

    fn schedule(&self) -> Value {
        let view = locked(&self.live.view);
        let desk = locked(&self.desk);
        let now = self.now(&view);
        let ms = |ns: u64| ns / 1_000_000;
        let latest = Self::latest_rounds(&view);
        let templates: Vec<Value> = self
            .calendar
            .templates
            .iter()
            .map(|(&template, t)| {
                let last = latest.get(&template);
                let next = last.map_or(now, |(_, at)| at.saturating_add(t.every).max(now));
                json!({
                    "template": template, "title": self.catalogue.title(catalogue::event_id(template, 1)),
                    "everySeconds": t.every / NANOS, "latestRound": last.map(|(r, _)| r), "nextRoundAt": ms(next),
                    "blocked": self.no_room(&view, template)
                })
            })
            .collect();
        let mut recent: Vec<(u64, u64)> = view
            .events
            .iter()
            .filter(|(&id, _)| !Self::is_parlay(&view, id))
            .filter_map(|(&id, e)| e.resolution.as_ref().map(|r| (r.at, id)))
            .collect();
        recent.sort_unstable_by(|a, b| b.cmp(a));
        let mut open: Vec<(u64, u64)> = view
            .events
            .iter()
            .filter(|(&id, e)| !e.resolved && !Self::is_parlay(&view, id))
            .map(|(&id, e)| {
                let start = self.timing(id).map_or(e.scheduled_start, |t| {
                    Calendar::start_at(
                        t,
                        &Facts { defined_at: e.defined_at, scheduled_start: e.scheduled_start, ..Facts::default() },
                    )
                });
                (e.started_at.unwrap_or(start), id)
            })
            .collect();
        open.sort_unstable();
        let events: Vec<Value> = open
            .iter()
            .chain(recent.iter().take(RECENT))
            .filter_map(|&(_, id)| self.event_json(&view, now, &desk, id))
            .collect();
        json!({
            "now": ms(now), "templates": templates, "events": events, "listing": desk.paused, "cases": desk.cases_paused,
            "disputeSeconds": self.calendar.timers.dispute / NANOS,
            "tiebreakSeconds": self.calendar.timers.tiebreak / NANOS,
            "escalationSeconds": self.calendar.timers.escalation / NANOS
        })
    }

    fn event_json(&self, view: &View, now: u64, desk: &Desk, event: u64) -> Option<Value> {
        let e = view.events.get(&event)?;
        let facts = Self::facts(view, desk, event)?;
        let ms = |ns: u64| ns / 1_000_000;
        let names = self.names(event, view);
        let timing = self.timing(event);
        let start = timing.map_or(e.scheduled_start, |t| Calendar::start_at(t, &facts));
        let ends = timing.map(|t| Calendar::end_at(t, e.started_at.unwrap_or(start)));
        let closes = timing.and_then(|t| Some(Calendar::close_at(t, e.ended_at.or(ends)?)));
        let reports = timing.and_then(|t| Some(self.calendar.report_at(t, e.ended_at.or(ends)?)));
        let case = desk.cases.get(&event);
        let status = match (&e.resolution, case) {
            (Some(r), _) if r.kind == "VOID" => "voided",
            (Some(_), _) => "settled",
            (None, Some(_)) => "resolving",
            (None, None) if e.ended && facts.tradable.is_empty() => "closed",
            (None, None) if e.ended => "ended",
            (None, None) if e.suspended => "suspended",
            (None, None) if e.started => "in play",
            (None, None) => "upcoming",
        };
        let (template, round) = split_event(event);
        let markets: Vec<Value> = e
            .instruments
            .iter()
            .filter_map(|i| {
                let m = view.instruments.get(i)?;
                Some(json!({"instrumentId": i, "label": self.catalogue.label(event, *i), "state": m.state.to_lowercase()}))
            })
            .collect();
        let resolution = e.resolution.as_ref().map(|r| {
            json!({
                "kind": r.kind.to_lowercase(), "winningState": r.winning_state, "caseId": r.case_id, "at": ms(r.at),
                "outcome": if r.kind == "VOID" { None } else { names.get(r.winning_state as usize) }
            })
        });
        Some(json!({
            "eventId": event, "title": self.catalogue.title(event), "template": template, "round": round,
            "outcomes": names, "markets": markets, "status": status, "onCalendar": timing.is_some(),
            "listedAt": ms(e.defined_at), "startsAt": (start > 0).then(|| ms(start)),
            "startedAt": e.started_at.map(ms), "endsAt": ends.map(ms), "endedAt": e.ended_at.map(ms),
            "closesAt": closes.map(ms), "reportsAt": reports.map(ms),
            "postponedUntil": desk.holds.get(&event).filter(|h| **h > now).map(|h| ms(*h)),
            "suspended": e.suspended, "suspendedByHand": facts.suspended_by_hand, "resolution": resolution,
            "case": case.map(|c| c.json(&names))
        }))
    }

    fn list_now(&self, template: u64) -> Result<Value, Refusal> {
        let (inputs, title) = {
            let view = locked(&self.live.view);
            if let Some(reason) = self.no_room(&view, template) {
                return refuse(400, reason);
            }
            let round = Self::latest_rounds(&view).get(&template).map_or(1, |(r, _)| r + 1);
            let lead = self.calendar.templates.get(&template).map_or(0, |t| t.lead);
            let inputs = self.catalogue.listing(template, round, self.now(&view) + lead)?;
            (inputs, self.catalogue.title(catalogue::event_id(template, round)))
        };
        self.send(vec![(format!("list {title} by hand"), inputs)])?;
        Ok(json!({"listed": title}))
    }

    fn event_action(&self, event: u64, action: &str, body: &Value) -> Result<Value, Refusal> {
        let mut batch = Batch::new();
        {
            let view = locked(&self.live.view);
            let mut desk = locked(&self.desk);
            let Some(e) = view.events.get(&event) else { return refuse(404, format!("no event {event}")) };
            if Self::is_parlay(&view, event) {
                return refuse(
                    400,
                    format!("event {event} is a parlay's: the parlay service settles it from its legs"),
                );
            }
            if e.resolved {
                return refuse(400, "the event is already resolved");
            }
            let now = self.now(&view);
            let names = self.names(event, &view);
            let facts = Self::facts(&view, &desk, event).unwrap_or_default();
            let timers = &self.calendar.timers;
            let states = e.state_count;
            let mut ingress = locked(&self.ingress);
            let ids = &mut ingress.1;
            let what = format!("{action} {event} by hand");
            match action {
                "start" | "suspend" | "resume" | "end" => {
                    let phase = match action {
                        "start" => "STARTED",
                        "suspend" => "SUSPENDED",
                        "resume" => "RESUMED",
                        _ => "ENDED",
                    };
                    match action {
                        "suspend" => {
                            self.save_manual(|s| s.suspended_by_hand(event, true))?;
                            desk.suspended_by_hand.insert(event);
                        }
                        "resume" => {
                            self.save_manual(|s| s.suspended_by_hand(event, false))?;
                            desk.suspended_by_hand.remove(&event);
                        }
                        _ => {}
                    }
                    if desk.holds.contains_key(&event) {
                        self.save_manual(|s| s.hold(event, None))?;
                    }
                    desk.holds.remove(&event);
                    batch.push((what, vec![Self::phase(ids, event, phase, "OPERATOR")]));
                }
                "close" if facts.tradable.is_empty() => return refuse(400, "no market of the event is open"),
                "close" => batch.push((what, Self::close(ids, &facts.tradable))),
                "propose" | "void" => {
                    let outcome = if action == "void" {
                        Outcome::Void
                    } else {
                        Outcome::State(
                            body["winningState"]
                                .as_u64()
                                .ok_or_else(|| Refusal(400, "winningState is required".into()))?,
                        )
                    };
                    let evidence =
                        body["evidence"].as_str().unwrap_or("proposed through the dummy oracle's control API");
                    let before = desk.cases.get(&event).cloned();
                    let case = desk.cases.entry(event).or_insert_with(|| Case::open(event, states, now));
                    case.operator_propose(outcome, format!("operator: {evidence}"), now, timers, &names)
                        .map_err(|e| Refusal(400, e))?;
                    if let Err(refusal) = self.save_case(&desk, event) {
                        match before {
                            Some(old) => desk.cases.insert(event, old),
                            None => desk.cases.remove(&event),
                        };
                        return Err(refusal);
                    }
                }
                "dispute" => {
                    let reason = body["reason"].as_str().unwrap_or("disputed through the control API");
                    let case =
                        desk.cases.get_mut(&event).ok_or_else(|| Refusal(400, "the event has no case".into()))?;
                    let before = case.clone();
                    case.dispute(reason, now).map_err(|e| Refusal(400, e))?;
                    if let Err(refusal) = self.save_case(&desk, event) {
                        desk.cases.insert(event, before);
                        return Err(refusal);
                    }
                }
                "postpone" => {
                    if e.started {
                        return refuse(400, "the event has started: suspend it instead");
                    }
                    let seconds = body["seconds"].as_u64().filter(|s| (1..=86_400).contains(s));
                    let seconds = seconds.ok_or_else(|| Refusal(400, "seconds must be from 1 to 86400".into()))?;
                    let from = self.timing(event).map_or(now, |t| Calendar::start_at(t, &facts)).max(now);
                    let until = desk.holds.get(&event).copied().unwrap_or(from).max(from) + seconds * NANOS;
                    self.save_manual(|s| s.hold(event, Some(until)))?;
                    desk.holds.insert(event, until);
                    eprintln!("oracle: postpone {event} by hand for {seconds} s");
                }
                _ => return refuse(404, format!("no event action {action}")),
            }
        }
        if !batch.is_empty() {
            let last = self.send(batch)?;
            self.await_feed(last);
        }
        let view = locked(&self.live.view);
        let now = self.now(&view);
        let desk = locked(&self.desk);
        Ok(self.event_json(&view, now, &desk, event).unwrap_or(Value::Null))
    }

    fn route(&self, method: &str, path: &str, body: &Value) -> Result<Value, Refusal> {
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        let number = |s: &str| s.parse::<u64>().map_err(|_| Refusal(404, format!("{s} is not an ID")));
        match (method, segments.as_slice()) {
            ("GET", ["oracle", "healthz"]) => {
                let database = match &self.store {
                    None => "none".to_string(),
                    Some(store) => store.ping().map_or_else(|e| format!("unavailable: {e}"), |()| "ok".to_string()),
                };
                Ok(json!({"ready": locked(&self.live.view).now > 0, "database": database}))
            }
            ("GET", ["oracle", "schedule"]) => Ok(self.schedule()),
            ("POST", ["oracle", "templates", template, "list"]) => {
                let template = number(template)?;
                if !self.catalogue.templates().contains(&template) {
                    return refuse(404, format!("no template {template}"));
                }
                self.list_now(template)
            }
            ("POST", ["oracle", "events", event, action]) => self.event_action(number(event)?, action, body),
            _ => refuse(404, "no such route"),
        }
    }
}

pub fn serve(listen: &str, oracle: Arc<Oracle>) -> Result<(), Error> {
    let listener = TcpListener::bind(listen)?;
    eprintln!("dummy oracle control API on http://{}", listener.local_addr()?);
    {
        let oracle = Arc::clone(&oracle);
        thread::spawn(move || oracle.run());
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let oracle = Arc::clone(&oracle);
        thread::spawn(move || {
            if let Err(e) = handle(&oracle, stream) {
                eprintln!("oracle API: {e}");
            }
        });
    }
    Ok(())
}

fn handle(oracle: &Oracle, stream: TcpStream) -> Result<(), Error> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("/").to_string());
    let mut length = 0usize;
    for _ in 0..100 {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim_end().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse()?;
            }
        }
    }
    if length > 1 << 16 {
        return crate::web::respond(stream, 413, "text/plain", b"too large");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let path = target.split_once('?').map_or(target.as_str(), |(p, _)| p);
    let request: Value = if body.is_empty() { json!({}) } else { serde_json::from_slice(&body).unwrap_or(Value::Null) };
    let (status, answer) = if request.is_null() {
        (400, json!({"error": "the body is not JSON"}))
    } else {
        match oracle.route(&method, path, &request) {
            Ok(v) => (200, v),
            Err(Refusal(status, message)) => (status, json!({"error": message})),
        }
    };
    crate::web::respond(stream, status, "application/json", answer.to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use venue_link::wire::{Frame, Unit};

    fn apply(view: &mut View, msg: Value, at: u64) {
        let mut input = msg;
        input["record"] = json!({"sequence": at, "sequencedAt": at, "sequencerGroup": 1});
        view.apply(&Unit {
            input: Frame::of(&input),
            results: vec![Frame::of(&json!({"msg": "CommandApplied", "outcome": "APPLIED"}))],
        });
    }

    #[test]
    fn a_parlays_event_is_no_templates_round() {
        let mut view = View::default();
        let round = catalogue::event_id(1, 3);
        apply(&mut view, json!({"msg": "DefineEvent", "eventId": round, "stateCount": 2, "scheduledStart": 0}), 1);
        let market = json!({"msg": "ListInstrument", "instrumentId": catalogue::instrument_id(round, 0), "eventId": round,
                            "instrumentClass": "BINARY", "stateMask": 2, "tickTableId": 1, "lotStep": 1, "legs": []});
        apply(&mut view, market, 2);
        // A parlay's event and instrument share one ID, hashed into the range from 2^52.
        let parlay = (1u64 << 52) + 12_345_678_901;
        apply(&mut view, json!({"msg": "DefineEvent", "eventId": parlay, "stateCount": 2, "scheduledStart": 0}), 3);
        let listing = json!({"msg": "ListInstrument", "instrumentId": parlay, "eventId": parlay,
                             "instrumentClass": "PARLAY", "stateMask": 2, "tickTableId": 2, "lotStep": 10, "legs": []});
        apply(&mut view, listing, 4);
        assert!(!Oracle::is_parlay(&view, round));
        assert!(Oracle::is_parlay(&view, parlay));
        let latest = Oracle::latest_rounds(&view);
        assert_eq!(latest.keys().copied().collect::<Vec<_>>(), vec![1], "only the template's round counts");
        assert_eq!(latest[&1].0, 3);
    }
}
