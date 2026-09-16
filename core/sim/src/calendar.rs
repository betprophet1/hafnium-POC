//! The dummy oracle's calendar (blueprint §16): when each template lists a new round, and what happens to a round
//! after that, all on sequencer time.
//!
//! A calendar is a JSON file (`deploy/charts/venue-mini/files/calendar.json`). Each template it names lists a round
//! every `everySeconds`; the round's event is scheduled to start `leadSeconds` after listing, is in play for
//! `runSeconds`, and its markets close `closeAfterSeconds` after it ends. Sources report an outcome
//! `reportAfterSeconds` after that, and resolution takes over (`resolution.rs`). Some rounds are suspended for a
//! while in play, some are abandoned and voided, and sometimes a source reports the wrong outcome; each of those is
//! a chance in thousandths, drawn from the calendar's seed and the event ID alone.
//!
//! Nothing here keeps a clock or remembers what it did. Every decision is a function of the calendar, the event's
//! facts as the feed recorded them (when it was defined, started and ended, which markets are still open) and the
//! sequencer's time now. A restarted oracle therefore picks up exactly where the journal says the venue is, and the
//! same seed makes the same choices for the same event on every run.

use crate::bots::Rng;
use crate::resolution::{Outcome, Report, Source};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use venue_link::wire::Error;

pub const NANOS: u64 = 1_000_000_000;

/// One template's cadence and timings, in nanoseconds, and its chances, in thousandths. The default is a template
/// the calendar does not run: no chances, so its sources report the truth.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Timing {
    pub every: u64,
    pub lead: u64,
    pub run: u64,
    pub close_after: u64,
    pub suspend_chance: u64,
    pub suspend_for: u64,
    pub void_chance: u64,
    pub error_chance: u64,
}

/// How long each stage of resolution may take, in nanoseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timers {
    /// From the markets closing to the sources reporting.
    pub report_after: u64,
    /// From the two sources disagreeing to the referee answering.
    pub tiebreak: u64,
    /// From a proposal being signed to its being sequenced, unless someone disputes it.
    pub dispute: u64,
    /// How long an escalated case waits for an operator before the auto-approver voids it.
    pub escalation: u64,
}

#[derive(Clone, Debug)]
pub struct Calendar {
    pub seed: u64,
    pub timers: Timers,
    pub templates: BTreeMap<u64, Timing>,
}

/// What the feed has recorded about an event, as the calendar needs it. Times are sequencer nanoseconds.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub defined_at: u64,
    /// As `DefineEvent` gave it; 0 for none.
    pub scheduled_start: u64,
    pub started_at: Option<u64>,
    pub suspended: bool,
    /// Suspended at any time so far, by the calendar or by hand: the calendar suspends a round at most once.
    pub suspended_ever: bool,
    /// Suspended by hand: only a person resumes it.
    pub suspended_by_hand: bool,
    pub ended_at: Option<u64>,
    /// Markets still open or halted, which closing would close.
    pub tradable: Vec<u64>,
    pub resolved: bool,
}

/// The next thing the calendar wants done to an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Start,
    Suspend,
    Resume,
    End,
    Close,
    /// The sources should report: open a resolution case if there is none.
    Report,
}

fn seconds(v: &Value, key: &str) -> Result<u64, Error> {
    let s = v[key].as_u64().ok_or_else(|| format!("calendar: {key} must be a whole number of seconds"))?;
    s.checked_mul(NANOS).ok_or_else(|| format!("calendar: {key} is too long").into())
}

fn per_mille(v: &Value, key: &str) -> Result<u64, Error> {
    match v.get(key) {
        None => Ok(0),
        Some(x) => x.as_u64().filter(|c| *c <= 1000).ok_or_else(|| format!("calendar: {key} must be 0 to 1000").into()),
    }
}

impl Calendar {
    pub fn load(path: &str) -> Result<Self, Error> {
        Self::parse(&fs::read_to_string(path)?)
    }

    pub fn parse(text: &str) -> Result<Self, Error> {
        let v: Value = serde_json::from_str(text)?;
        let r = &v["resolution"];
        let timers = Timers {
            report_after: seconds(r, "reportAfterSeconds")?,
            tiebreak: seconds(r, "tiebreakSeconds")?,
            dispute: seconds(r, "disputeSeconds")?,
            escalation: seconds(r, "escalationSeconds")?,
        };
        let mut templates = BTreeMap::new();
        for t in v["templates"].as_array().into_iter().flatten() {
            let template = t["template"].as_u64().ok_or("calendar: every entry needs a template")?;
            let timing = Timing {
                every: seconds(t, "everySeconds")?,
                lead: seconds(t, "leadSeconds")?,
                run: seconds(t, "runSeconds")?,
                close_after: seconds(t, "closeAfterSeconds")?,
                suspend_chance: per_mille(t, "suspendChance")?,
                suspend_for: if t.get("suspendSeconds").is_some() { seconds(t, "suspendSeconds")? } else { 0 },
                void_chance: per_mille(t, "voidChance")?,
                error_chance: per_mille(t, "errorChance")?,
            };
            // A cadence of zero would list a round on every pass, and a round that never runs would end before
            // anyone could trade it.
            if timing.every == 0 || timing.run == 0 {
                return Err(format!("calendar: template {template} needs everySeconds and runSeconds above 0").into());
            }
            if templates.insert(template, timing).is_some() {
                return Err(format!("calendar: template {template} appears twice").into());
            }
        }
        Ok(Self { seed: v["seed"].as_u64().unwrap_or(0), timers, templates })
    }

    /// A number drawn for one purpose (`salt`) about one event: the same on every run with the same seed, and
    /// independent of the order events are looked at in.
    pub fn draw(&self, event: u64, salt: u64) -> u64 {
        let mut rng =
            Rng::new(self.seed ^ event.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt.wrapping_mul(0xD6E8_FEB8_6659_FD93));
        rng.next_u64();
        rng.next_u64()
    }

    fn chance(&self, event: u64, salt: u64, per_mille: u64) -> bool {
        self.draw(event, salt) % 1000 < per_mille
    }

    /// The round to list next and its scheduled start, if one is due. `latest` is the template's highest round so
    /// far and when its event was defined. A template with no rounds lists one at once; after that one every
    /// `every`, counted from when the last one was actually listed, so an oracle that was down lists one round when
    /// it returns rather than every round it missed.
    pub fn listing_due(t: &Timing, latest: Option<(u64, u64)>, now: u64) -> Option<(u64, u64)> {
        match latest {
            None => Some((1, now + t.lead)),
            Some((round, defined_at)) if now >= defined_at.saturating_add(t.every) => Some((round + 1, now + t.lead)),
            Some(_) => None,
        }
    }

    /// When the event is due to start: its scheduled start, or, for an event listed without one (the seed lists
    /// round 1 that way), the template's lead after it was defined.
    pub fn start_at(t: &Timing, f: &Facts) -> u64 {
        if f.scheduled_start == 0 {
            f.defined_at.saturating_add(t.lead)
        } else {
            f.scheduled_start
        }
    }

    /// The window in which a round is suspended, if its draw says it is: from a third of the way through its run,
    /// for `suspend_for`, never past the end.
    pub fn suspension(&self, t: &Timing, event: u64, started_at: u64) -> Option<(u64, u64)> {
        if !self.chance(event, SALT_SUSPEND, t.suspend_chance) {
            return None;
        }
        let from = started_at + t.run / 3;
        let until = (from + t.suspend_for).min(started_at + t.run);
        (until > from).then_some((from, until))
    }

    /// When the event is due to end, once it has started.
    pub const fn end_at(t: &Timing, started_at: u64) -> u64 {
        started_at.saturating_add(t.run)
    }

    /// When the markets are due to close, and when the sources report, once the event has ended.
    pub const fn close_at(t: &Timing, ended_at: u64) -> u64 {
        ended_at.saturating_add(t.close_after)
    }

    pub const fn report_at(&self, t: &Timing, ended_at: u64) -> u64 {
        Self::close_at(t, ended_at).saturating_add(self.timers.report_after)
    }

    /// The next step due for an event at `now`, if any. `hold_until` is a postponement: nothing is due before it.
    /// Steps come one at a time, each once the feed shows the one before it, so an oracle that fell behind catches
    /// up in order rather than all at once.
    pub fn due(&self, t: &Timing, event: u64, f: &Facts, now: u64, hold_until: u64) -> Option<Step> {
        if f.resolved || now < hold_until {
            return None;
        }
        let Some(started_at) = f.started_at else {
            return (now >= Self::start_at(t, f)).then_some(Step::Start);
        };
        let Some(ended_at) = f.ended_at else {
            if now >= Self::end_at(t, started_at) {
                return Some(Step::End);
            }
            let (from, until) = self.suspension(t, event, started_at)?;
            return match f.suspended {
                false if !f.suspended_ever && now >= from && now < until => Some(Step::Suspend),
                true if !f.suspended_by_hand && now >= until => Some(Step::Resume),
                _ => None,
            };
        };
        if !f.tradable.is_empty() {
            return (now >= Self::close_at(t, ended_at)).then_some(Step::Close);
        }
        (now >= self.report_at(t, ended_at)).then_some(Step::Report)
    }

    /// What really happened, for the dummy: an abandoned event now and then, otherwise a state drawn at random.
    pub fn truth(&self, t: &Timing, event: u64, states: u64) -> Outcome {
        if self.chance(event, SALT_VOID, t.void_chance) || states == 0 {
            Outcome::Void
        } else {
            Outcome::State(self.draw(event, SALT_TRUTH) % states)
        }
    }

    /// What one source reports about an event. The two registered sources each misread a result now and then,
    /// independently, and report some other state; the referee never does. An abandoned event is reported as
    /// abandoned by all of them.
    pub fn report(&self, t: &Timing, event: u64, states: &[String], source: Source, now: u64) -> Report {
        let count = states.len() as u64;
        let truth = self.truth(t, event, count);
        let salt = match source {
            Source::Alpha => SALT_ALPHA,
            Source::Beta => SALT_BETA,
            Source::Referee | Source::Operator | Source::AutoApprover => SALT_REFEREE,
        };
        let draw = self.draw(event, salt);
        let misread = matches!(source, Source::Alpha | Source::Beta) && self.chance(event, salt + 1, t.error_chance);
        let outcome = match truth {
            Outcome::State(s) if misread && count > 1 => Outcome::State((s + 1 + draw % (count - 1)) % count),
            other => other,
        };
        let reading = match outcome {
            Outcome::Void => "the event was abandoned".to_string(),
            Outcome::State(s) => format!("the result was {}", states.get(s as usize).map_or("?", String::as_str)),
        };
        let evidence = format!("{}: synthetic observation {:08x} says {reading}", source.name(), draw >> 32);
        Report { source, outcome, evidence, at: now }
    }
}

const SALT_SUSPEND: u64 = 1;
const SALT_VOID: u64 = 2;
const SALT_TRUTH: u64 = 3;
const SALT_ALPHA: u64 = 10;
const SALT_BETA: u64 = 20;
const SALT_REFEREE: u64 = 30;

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = NANOS;

    fn calendar(seed: u64) -> Calendar {
        Calendar::parse(&format!(
            r#"{{"seed": {seed},
                "resolution": {{"reportAfterSeconds": 5, "tiebreakSeconds": 15, "disputeSeconds": 30, "escalationSeconds": 60}},
                "templates": [{{"template": 1, "everySeconds": 240, "leadSeconds": 45, "runSeconds": 120,
                                "closeAfterSeconds": 10, "suspendChance": 1000, "suspendSeconds": 20,
                                "voidChance": 0, "errorChance": 0}}]}}"#
        ))
        .expect("a calendar")
    }

    fn timing(c: &Calendar) -> Timing {
        c.templates[&1].clone()
    }

    /// The chart's calendar reads, names only templates the catalogue lists, and lets every round settle before the
    /// next of its market is listed, so the demo never has more than a couple of rounds of a market open at once.
    #[test]
    fn the_charts_calendar_is_one_the_demo_can_run() {
        let files = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/charts/venue-mini/files/");
        let c = Calendar::load(&format!("{files}calendar.json")).expect("the chart's calendar");
        let catalogue = venue_link::catalogue::Catalogue::load(&format!("{files}markets.json")).expect("the catalogue");
        assert_eq!(c.templates.keys().copied().collect::<Vec<_>>(), catalogue.templates());
        for (template, t) in &c.templates {
            let settled = t.lead + t.run + t.suspend_for + t.close_after + c.timers.report_after + c.timers.dispute;
            assert!(settled < t.every, "template {template}: a round takes {settled} ns, longer than its cadence");
        }
    }

    #[test]
    fn the_file_reads_as_nanoseconds_and_thousandths() {
        let c = calendar(7);
        let t = timing(&c);
        assert_eq!((t.every, t.lead, t.run, t.close_after), (240 * S, 45 * S, 120 * S, 10 * S));
        assert_eq!((t.suspend_chance, t.suspend_for), (1000, 20 * S));
        assert_eq!(c.timers, Timers { report_after: 5 * S, tiebreak: 15 * S, dispute: 30 * S, escalation: 60 * S });
    }

    #[test]
    fn a_calendar_that_could_never_run_is_refused() {
        let zero = r#"{"resolution": {"reportAfterSeconds": 1, "tiebreakSeconds": 1, "disputeSeconds": 1, "escalationSeconds": 1},
                       "templates": [{"template": 1, "everySeconds": 0, "leadSeconds": 1, "runSeconds": 1, "closeAfterSeconds": 0}]}"#;
        assert!(Calendar::parse(zero).is_err());
        let chance = zero.replace("\"everySeconds\": 0", "\"everySeconds\": 5, \"voidChance\": 1001");
        assert!(Calendar::parse(&chance).is_err());
        assert!(Calendar::parse(r#"{"templates": []}"#).is_err(), "resolution timers are required");
    }

    #[test]
    fn rounds_list_on_the_cadence_counted_from_the_last_listing() {
        let t = timing(&calendar(1));
        assert_eq!(Calendar::listing_due(&t, None, 1_000 * S), Some((1, 1_045 * S)));
        assert_eq!(Calendar::listing_due(&t, Some((4, 1_000 * S)), 1_239 * S), None);
        assert_eq!(Calendar::listing_due(&t, Some((4, 1_000 * S)), 1_240 * S), Some((5, 1_285 * S)));
        // Down for an hour: one round now, not fifteen.
        assert_eq!(Calendar::listing_due(&t, Some((4, 1_000 * S)), 4_600 * S), Some((5, 4_645 * S)));
    }

    #[test]
    fn a_round_goes_through_its_stages_in_order_and_on_time() {
        let c = calendar(1);
        let t = timing(&c);
        let mut f = Facts { defined_at: 100 * S, scheduled_start: 145 * S, tradable: vec![10], ..Facts::default() };
        assert_eq!(c.due(&t, 7, &f, 144 * S, 0), None);
        assert_eq!(c.due(&t, 7, &f, 145 * S, 0), Some(Step::Start));
        f.started_at = Some(146 * S);
        // Suspended from a third of the way through the run (40 s) for 20 s.
        assert_eq!(c.suspension(&t, 7, 146 * S), Some((186 * S, 206 * S)));
        assert_eq!(c.due(&t, 7, &f, 185 * S, 0), None);
        assert_eq!(c.due(&t, 7, &f, 186 * S, 0), Some(Step::Suspend));
        f.suspended = true;
        f.suspended_ever = true;
        assert_eq!(c.due(&t, 7, &f, 205 * S, 0), None);
        assert_eq!(c.due(&t, 7, &f, 206 * S, 0), Some(Step::Resume));
        f.suspended = false;
        // Resumed, so it is not suspended again.
        assert_eq!(c.due(&t, 7, &f, 207 * S, 0), None);
        assert_eq!(c.due(&t, 7, &f, 266 * S, 0), Some(Step::End));
        f.ended_at = Some(266 * S);
        assert_eq!(c.due(&t, 7, &f, 275 * S, 0), None);
        assert_eq!(c.due(&t, 7, &f, 276 * S, 0), Some(Step::Close));
        f.tradable.clear();
        assert_eq!(c.due(&t, 7, &f, 280 * S, 0), None);
        assert_eq!(c.due(&t, 7, &f, 281 * S, 0), Some(Step::Report));
        f.resolved = true;
        assert_eq!(c.due(&t, 7, &f, 1_000 * S, 0), None);
    }

    #[test]
    fn stages_count_from_what_actually_happened() {
        let c = calendar(1);
        let t = timing(&c);
        // Started by hand a minute early: it ends a run after that, not after the scheduled start.
        let f = Facts { defined_at: 0, scheduled_start: 200 * S, started_at: Some(140 * S), ..Facts::default() };
        assert_eq!(c.due(&t, 3, &f, 260 * S, 0), Some(Step::End));
        // The seed lists round 1 without a scheduled start: it starts a lead after it was defined.
        let seeded = Facts { defined_at: 10 * S, ..Facts::default() };
        assert_eq!(Calendar::start_at(&t, &seeded), 55 * S);
    }

    #[test]
    fn the_calendar_leaves_a_suspension_by_hand_alone() {
        let c = calendar(1);
        let t = timing(&c);
        let mut f = Facts { started_at: Some(0), suspended: true, suspended_ever: true, ..Facts::default() };
        f.suspended_by_hand = true;
        assert_eq!(c.due(&t, 7, &f, 70 * S, 0), None, "past the window, but a person suspended it");
        // Resumed by hand inside the window: the calendar does not suspend it again.
        let resumed = Facts { started_at: Some(0), suspended_ever: true, ..Facts::default() };
        assert_eq!(c.due(&t, 7, &resumed, 45 * S, 0), None);
        assert_eq!(c.due(&t, 7, &resumed, 120 * S, 0), Some(Step::End), "and it still ends on time");
    }

    #[test]
    fn a_postponement_holds_every_stage_until_it_ends() {
        let c = calendar(1);
        let t = timing(&c);
        let f = Facts { defined_at: 100 * S, scheduled_start: 145 * S, ..Facts::default() };
        assert_eq!(c.due(&t, 7, &f, 150 * S, 200 * S), None);
        assert_eq!(c.due(&t, 7, &f, 200 * S, 200 * S), Some(Step::Start));
    }

    #[test]
    fn the_same_seed_makes_the_same_choices_and_another_seed_others() {
        let states = vec!["No".to_string(), "Yes".to_string()];
        let (a, b) = (calendar(5), calendar(5));
        let t = timing(&a);
        for event in 1_000_001..1_000_050 {
            assert_eq!(a.truth(&t, event, 2), b.truth(&t, event, 2));
            assert_eq!(a.report(&t, event, &states, Source::Beta, 0), b.report(&t, event, &states, Source::Beta, 0));
        }
        let other = calendar(6);
        assert!((1_000_001..1_000_050).any(|e| a.truth(&t, e, 2) != other.truth(&t, e, 2)));
    }

    #[test]
    fn sources_misread_at_about_their_error_chance_and_the_referee_never() {
        let c = calendar(9);
        let mut t = timing(&c);
        t.error_chance = 200;
        t.void_chance = 100;
        let states: Vec<String> = ["Arsenal", "Draw", "Chelsea"].iter().map(|s| s.to_string()).collect();
        let (mut wrong, mut voids, n) = (0, 0, 4_000u64);
        for event in 0..n {
            let truth = c.truth(&t, event, 3);
            voids += u64::from(truth == Outcome::Void);
            let alpha = c.report(&t, event, &states, Source::Alpha, 0);
            wrong += u64::from(alpha.outcome != truth);
            assert_eq!(c.report(&t, event, &states, Source::Referee, 0).outcome, truth);
            if truth == Outcome::Void {
                assert_eq!(alpha.outcome, Outcome::Void, "nobody misreads an abandonment");
            }
            if let Outcome::State(s) = alpha.outcome {
                assert!(s < 3);
            }
        }
        // 20% of the 90% that were not voided, and 10% voided, give or take.
        assert!((600..840).contains(&wrong), "{wrong}");
        assert!((320..480).contains(&voids), "{voids}");
    }

    #[test]
    fn evidence_names_the_source_and_what_it_read() {
        let c = calendar(3);
        let t = timing(&c);
        let states = vec!["No rain".to_string(), "Rain".to_string()];
        let r = c.report(&t, 1_000_002, &states, Source::Alpha, 42);
        assert!(r.evidence.starts_with("alpha: synthetic observation "), "{}", r.evidence);
        assert!(r.evidence.contains("rain") || r.evidence.contains("Rain"), "{}", r.evidence);
        assert_eq!(r.at, 42);
    }
}
