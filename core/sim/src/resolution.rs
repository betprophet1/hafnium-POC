//! Resolution cases for the dummy oracle (blueprint §11 and §16): reports from sources, a proposal with evidence,
//! two-of-three sign-off by the auto-approver, a dispute window, and only then a `ResolveEvent`.
//!
//! # The two oracles and the referee
//!
//! Every demo market registers the same sources at listing: two independent primary sources, `alpha` and `beta`, and
//! a `referee` as the fallback §11 asks for. Each is a dummy that reads the event's synthetic result from the
//! calendar's seed (`calendar.rs`), and each of the two primaries misreads it now and then, independently, so they
//! sometimes disagree. The flow is:
//!
//! 1. When the event's markets have closed, `alpha` and `beta` report, each with evidence.
//! 2. If they agree, their outcome becomes a proposal naming both.
//! 3. If they disagree, the case waits a short tiebreak and asks the `referee`. Its report makes two of three agree,
//!    and that outcome becomes the proposal. In an event with three or more states it can agree with neither: the
//!    case is escalated to an operator.
//! 4. The auto-approver signs a proposal with two test-operator credentials, after checking what a person signing
//!    would check: the outcome is one of the event's states, and at least two sources back it or an operator put it
//!    forward. A proposal it refuses is escalated.
//! 5. A signed proposal waits out the dispute window. Anyone may dispute it through the control API, which escalates
//!    the case; otherwise, when the window closes, the oracle sequences `ResolveEvent` with the case's ID.
//! 6. An escalated case waits for an operator to propose an outcome or a void (which goes through steps 4 and 5
//!    again). If nobody does before the escalation timer runs out, the auto-approver proposes a void: a market whose
//!    result nobody can establish is reversed, never settled on a guess (§11).
//!
//! Nothing here reads a clock or does I/O: every change takes the time it happens at, so the whole machine is
//! tested on made-up times.

use crate::calendar::Timers;
use serde_json::{json, Value};

/// A source of results, as registered at listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Alpha,
    Beta,
    Referee,
    /// A person, through the control API.
    Operator,
    /// The auto-approver itself, voiding an escalation nobody answered.
    AutoApprover,
}

impl Source {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Alpha => "alpha",
            Self::Beta => "beta",
            Self::Referee => "referee",
            Self::Operator => "operator",
            Self::AutoApprover => "auto-approver",
        }
    }
}

/// What a source says happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The event's state that occurred, as `ResolveEvent.winningState`.
    State(u64),
    /// Abandoned, postponed for good or otherwise without a result: every trade is reversed (E-15.4).
    Void,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub source: Source,
    pub outcome: Outcome,
    pub evidence: String,
    pub at: u64,
}

/// The two credentials the auto-approver holds. Two distinct signatures make the two of three that §11 requires; the
/// third would be a person's.
pub const APPROVERS: [&str; 2] = ["auto-approver-1", "auto-approver-2"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub outcome: Outcome,
    /// The sources that back it.
    pub sources: Vec<Source>,
    pub evidence: Vec<String>,
    pub signatures: Vec<&'static str>,
    pub signed_at: u64,
    pub window_ends: u64,
    /// Which proposal of the case this is, from 1: part of the `ResolveEvent` command ID and of the case ID.
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Waiting for both primary sources.
    Collecting,
    /// The primaries disagree; the referee is asked once `since + tiebreak` has passed.
    Tiebreak { since: u64 },
    /// Waiting for an operator; voided by the auto-approver once `since + escalation` has passed.
    Escalated { since: u64, reason: String },
    /// Signed; disputable until the window ends.
    Proposed(Proposal),
    /// The window closed undisputed and `ResolveEvent` has been sent; waiting for the feed to show it applied.
    Sequencing(Proposal),
    /// The feed shows the event resolved.
    Settled(Proposal),
}

/// What the oracle must do for a case, now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    AskReferee,
    /// Send `ResolveEvent` for this proposal. Its command ID is fixed per revision, so sending it again is harmless.
    Sequence(Proposal),
}

#[derive(Clone, Debug)]
pub struct Case {
    pub event: u64,
    pub states: u64,
    pub opened_at: u64,
    pub reports: Vec<Report>,
    pub stage: Stage,
    /// What happened, oldest first, for the page.
    pub history: Vec<(u64, String)>,
    revisions: u64,
}

fn describe(outcome: Outcome, names: &[String]) -> String {
    match outcome {
        Outcome::Void => "void".to_string(),
        Outcome::State(s) => names.get(s as usize).cloned().unwrap_or_else(|| format!("state {s}")),
    }
}

impl Case {
    pub fn open(event: u64, states: u64, now: u64) -> Self {
        Self {
            event,
            states,
            opened_at: now,
            reports: Vec::new(),
            stage: Stage::Collecting,
            history: vec![(now, "case opened".to_string())],
            revisions: 0,
        }
    }

    /// The case ID `ResolveEvent` carries: the event and the revision that settled it, so the journal says which
    /// proposal of which case caused a resolution.
    pub const fn case_id(event: u64, revision: u64) -> u64 {
        event * 100 + revision
    }

    fn note(&mut self, now: u64, what: String) {
        self.history.push((now, what));
    }

    fn reported(&self, source: Source) -> Option<&Report> {
        self.reports.iter().rev().find(|r| r.source == source)
    }

    pub const fn is_final(&self) -> bool {
        matches!(self.stage, Stage::Sequencing(_) | Stage::Settled(_))
    }

    /// A source's report. Once both primaries are in, the case either proposes or goes to the referee; the referee's
    /// report either proposes or escalates.
    pub fn report(&mut self, report: Report, timers: &Timers, names: &[String]) {
        let now = report.at;
        let source = report.source;
        self.note(now, format!("{} reports {}", source.name(), describe(report.outcome, names)));
        self.reports.push(report);
        match (&self.stage, source) {
            (Stage::Collecting, Source::Alpha | Source::Beta) => {
                let (Some(a), Some(b)) = (self.reported(Source::Alpha), self.reported(Source::Beta)) else { return };
                if a.outcome == b.outcome {
                    let (outcome, evidence) = (a.outcome, vec![a.evidence.clone(), b.evidence.clone()]);
                    self.propose(outcome, vec![Source::Alpha, Source::Beta], evidence, now, timers, names);
                } else {
                    self.note(now, "alpha and beta disagree: asking the referee".to_string());
                    self.stage = Stage::Tiebreak { since: now };
                }
            }
            (Stage::Tiebreak { .. }, Source::Referee) => {
                let r = self.reports.last().cloned();
                let agreeing = [Source::Alpha, Source::Beta]
                    .into_iter()
                    .find(|s| r.as_ref().is_some_and(|r| self.reported(*s).is_some_and(|x| x.outcome == r.outcome)));
                match (r, agreeing) {
                    (Some(r), Some(with)) => {
                        let other = self.reported(with).map(|x| x.evidence.clone()).unwrap_or_default();
                        self.propose(
                            r.outcome,
                            vec![with, Source::Referee],
                            vec![other, r.evidence],
                            now,
                            timers,
                            names,
                        );
                    }
                    _ => self.escalate(now, "alpha, beta and the referee all disagree".to_string()),
                }
            }
            // A late or repeated report changes nothing once the case has moved on.
            _ => {}
        }
    }

    /// An operator's proposal: an outcome, or a void, with the operator's evidence. Replaces any proposal still in
    /// its dispute window, which then starts again.
    pub fn operator_propose(
        &mut self,
        outcome: Outcome,
        evidence: String,
        now: u64,
        timers: &Timers,
        names: &[String],
    ) -> Result<(), String> {
        if self.is_final() {
            return Err("the case has already been sent for sequencing".to_string());
        }
        self.reports.push(Report { source: Source::Operator, outcome, evidence: evidence.clone(), at: now });
        self.note(now, format!("operator proposes {}", describe(outcome, names)));
        self.propose(outcome, vec![Source::Operator], vec![evidence], now, timers, names);
        Ok(())
    }

    /// A dispute: only a proposal in its dispute window can be disputed, and doing so escalates the case.
    pub fn dispute(&mut self, reason: &str, now: u64) -> Result<(), String> {
        match self.stage {
            Stage::Proposed(ref p) if now < p.window_ends => {
                self.escalate(now, format!("disputed: {reason}"));
                Ok(())
            }
            _ => Err("only a proposal in its dispute window can be disputed".to_string()),
        }
    }

    fn escalate(&mut self, now: u64, reason: String) {
        self.note(now, format!("escalated: {reason}"));
        self.stage = Stage::Escalated { since: now, reason };
    }

    /// The auto-approver's checks and signatures (step 4 of the module comment).
    pub fn approve(outcome: Outcome, sources: &[Source], states: u64) -> Result<Vec<&'static str>, String> {
        if let Outcome::State(s) = outcome {
            if s >= states {
                return Err(format!("state {s} is not one of the event's {states} states"));
            }
        }
        let by_policy = matches!((outcome, sources), (_, [Source::Operator]) | (Outcome::Void, [Source::AutoApprover]));
        let independent =
            sources.iter().filter(|s| matches!(s, Source::Alpha | Source::Beta | Source::Referee)).count();
        if independent < 2 && !by_policy {
            return Err("fewer than two sources back the outcome".to_string());
        }
        Ok(APPROVERS.to_vec())
    }

    fn propose(
        &mut self,
        outcome: Outcome,
        sources: Vec<Source>,
        evidence: Vec<String>,
        now: u64,
        timers: &Timers,
        names: &[String],
    ) {
        match Self::approve(outcome, &sources, self.states) {
            Ok(signatures) => {
                self.revisions += 1;
                let backers: Vec<&str> = sources.iter().map(|s| s.name()).collect();
                self.note(
                    now,
                    format!(
                        "proposal {} of {} by {}, signed by {}; dispute window open",
                        self.revisions,
                        describe(outcome, names),
                        backers.join(" and "),
                        signatures.join(" and ")
                    ),
                );
                self.stage = Stage::Proposed(Proposal {
                    outcome,
                    sources,
                    evidence,
                    signatures,
                    signed_at: now,
                    window_ends: now.saturating_add(timers.dispute),
                    revision: self.revisions,
                });
            }
            Err(refusal) => self.escalate(now, format!("the auto-approver refused: {refusal}")),
        }
    }

    /// What is due at `now`. Closing a dispute window moves the case to `Sequencing` and returns the proposal to
    /// sequence; an escalation nobody answered becomes a void proposal.
    pub fn tick(&mut self, now: u64, timers: &Timers, names: &[String]) -> Option<Action> {
        match &self.stage {
            Stage::Tiebreak { since } if now >= since.saturating_add(timers.tiebreak) => {
                (self.reported(Source::Referee).is_none()).then_some(Action::AskReferee)
            }
            Stage::Escalated { since, .. } if now >= since.saturating_add(timers.escalation) => {
                self.note(now, "no operator answered the escalation: the auto-approver proposes a void".to_string());
                let evidence = "escalation timed out without an established result".to_string();
                self.propose(Outcome::Void, vec![Source::AutoApprover], vec![evidence], now, timers, names);
                None
            }
            Stage::Proposed(p) if now >= p.window_ends => {
                let p = p.clone();
                self.note(now, format!("dispute window closed: sequencing proposal {}", p.revision));
                self.stage = Stage::Sequencing(p.clone());
                Some(Action::Sequence(p))
            }
            _ => None,
        }
    }

    /// The feed shows the event resolved. A case settles with its own proposal; an event resolved some other way
    /// (straight through the ingress) closes the case with what the feed says.
    pub fn settled(&mut self, outcome: Outcome, now: u64, names: &[String]) {
        if matches!(self.stage, Stage::Settled(_)) {
            return;
        }
        let proposal = match &self.stage {
            Stage::Sequencing(p) if p.outcome == outcome => p.clone(),
            _ => Proposal {
                outcome,
                sources: Vec::new(),
                evidence: vec!["resolved outside this oracle".to_string()],
                signatures: Vec::new(),
                signed_at: now,
                window_ends: now,
                revision: 0,
            },
        };
        self.note(now, format!("settled as {}", describe(outcome, names)));
        self.stage = Stage::Settled(proposal);
    }

    /// The case as the control API shows it. Times are milliseconds since the epoch.
    pub fn json(&self, names: &[String]) -> Value {
        let ms = |ns: u64| ns / 1_000_000;
        let outcome = |o: Outcome| match o {
            Outcome::Void => json!({"kind": "void"}),
            Outcome::State(s) => json!({"kind": "resolved", "winningState": s, "name": describe(o, names)}),
        };
        let proposal = |p: &Proposal| {
            json!({
                "revision": p.revision, "outcome": outcome(p.outcome),
                "sources": p.sources.iter().map(|s| s.name()).collect::<Vec<_>>(), "evidence": p.evidence,
                "signatures": p.signatures, "signedAt": ms(p.signed_at), "windowEndsAt": ms(p.window_ends)
            })
        };
        let (stage, detail) = match &self.stage {
            Stage::Collecting => ("collecting", json!({})),
            Stage::Tiebreak { since } => ("tiebreak", json!({"since": ms(*since)})),
            Stage::Escalated { since, reason } => ("escalated", json!({"since": ms(*since), "reason": reason})),
            Stage::Proposed(p) => ("proposed", json!({"proposal": proposal(p)})),
            Stage::Sequencing(p) => ("sequencing", json!({"proposal": proposal(p)})),
            Stage::Settled(p) => ("settled", json!({"proposal": proposal(p)})),
        };
        json!({
            "eventId": self.event, "openedAt": ms(self.opened_at), "stage": stage, "detail": detail,
            "reports": self.reports.iter().map(|r| json!({
                "source": r.source.name(), "outcome": outcome(r.outcome), "evidence": r.evidence, "at": ms(r.at)
            })).collect::<Vec<_>>(),
            "history": self.history.iter().map(|(at, what)| json!({"at": ms(*at), "what": what})).collect::<Vec<_>>()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn timers() -> Timers {
        Timers { report_after: 5 * S, tiebreak: 15 * S, dispute: 30 * S, escalation: 60 * S }
    }

    fn names() -> Vec<String> {
        ["Arsenal", "Draw", "Chelsea"].iter().map(|s| s.to_string()).collect()
    }

    fn report(source: Source, outcome: Outcome, at: u64) -> Report {
        Report { source, outcome, evidence: format!("{} saw it", source.name()), at }
    }

    fn proposal(case: &Case) -> Proposal {
        match &case.stage {
            Stage::Proposed(p) | Stage::Sequencing(p) | Stage::Settled(p) => p.clone(),
            other => panic!("no proposal: {other:?}"),
        }
    }

    #[test]
    fn agreeing_sources_are_proposed_signed_held_and_then_sequenced() {
        let (t, n) = (timers(), names());
        let mut case = Case::open(2_000_001, 3, 100 * S);
        case.report(report(Source::Alpha, Outcome::State(2), 100 * S), &t, &n);
        assert_eq!(case.stage, Stage::Collecting, "one source is not enough");
        case.report(report(Source::Beta, Outcome::State(2), 101 * S), &t, &n);
        let p = proposal(&case);
        assert_eq!(p.outcome, Outcome::State(2));
        assert_eq!(p.sources, vec![Source::Alpha, Source::Beta]);
        assert_eq!(p.evidence.len(), 2);
        assert_eq!(p.signatures, APPROVERS.to_vec());
        assert_eq!(p.window_ends, 131 * S);
        assert_eq!(case.tick(130 * S, &t, &n), None, "the dispute window is still open");
        assert_eq!(case.tick(131 * S, &t, &n), Some(Action::Sequence(p.clone())));
        assert!(matches!(case.stage, Stage::Sequencing(_)));
        assert_eq!(case.tick(200 * S, &t, &n), None, "sequenced once");
        case.settled(Outcome::State(2), 132 * S, &n);
        assert_eq!(case.stage, Stage::Settled(p));
        assert_eq!(Case::case_id(2_000_001, 1), 200_000_101);
    }

    #[test]
    fn disagreement_goes_to_the_referee_after_the_tiebreak_and_two_of_three_decide() {
        let (t, n) = (timers(), names());
        let mut case = Case::open(7, 3, 0);
        case.report(report(Source::Alpha, Outcome::State(0), 0), &t, &n);
        case.report(report(Source::Beta, Outcome::State(1), 0), &t, &n);
        assert_eq!(case.stage, Stage::Tiebreak { since: 0 });
        assert_eq!(case.tick(14 * S, &t, &n), None);
        assert_eq!(case.tick(15 * S, &t, &n), Some(Action::AskReferee));
        case.report(report(Source::Referee, Outcome::State(1), 15 * S), &t, &n);
        let p = proposal(&case);
        assert_eq!((p.outcome, p.sources.clone()), (Outcome::State(1), vec![Source::Beta, Source::Referee]));
        assert_eq!(p.evidence, vec!["beta saw it".to_string(), "referee saw it".to_string()]);
        assert_eq!(p.window_ends, 45 * S);
    }

    #[test]
    fn three_different_answers_escalate_and_an_unanswered_escalation_is_voided() {
        let (t, n) = (timers(), names());
        let mut case = Case::open(7, 3, 0);
        case.report(report(Source::Alpha, Outcome::State(0), 0), &t, &n);
        case.report(report(Source::Beta, Outcome::State(1), 0), &t, &n);
        case.report(report(Source::Referee, Outcome::State(2), 15 * S), &t, &n);
        assert!(matches!(case.stage, Stage::Escalated { since, .. } if since == 15 * S));
        assert_eq!(case.tick(74 * S, &t, &n), None);
        assert_eq!(case.tick(75 * S, &t, &n), None, "the void is proposed, not sequenced at once");
        let p = proposal(&case);
        assert_eq!((p.outcome, p.window_ends, p.sources.clone()), (Outcome::Void, 105 * S, vec![Source::AutoApprover]));
        assert!(matches!(case.tick(105 * S, &t, &n), Some(Action::Sequence(v)) if v.outcome == Outcome::Void));
    }

    #[test]
    fn a_dispute_in_the_window_escalates_and_an_operator_proposal_restarts_the_window() {
        let (t, n) = (timers(), names());
        let mut case = Case::open(7, 3, 0);
        case.report(report(Source::Alpha, Outcome::State(0), 0), &t, &n);
        case.report(report(Source::Beta, Outcome::State(0), 0), &t, &n);
        case.dispute("the match was replayed", 10 * S).expect("in the window");
        assert!(matches!(case.stage, Stage::Escalated { .. }));
        assert_eq!(case.tick(31 * S, &t, &n), None, "nothing is sequenced while escalated");
        case.operator_propose(Outcome::State(1), "replay result".into(), 40 * S, &t, &n).expect("open case");
        let p = proposal(&case);
        assert_eq!((p.outcome, p.revision, p.window_ends), (Outcome::State(1), 2, 70 * S));
        assert!(case.dispute("late", 70 * S).is_err(), "the window has closed");
        assert!(matches!(case.tick(70 * S, &t, &n), Some(Action::Sequence(p)) if p.revision == 2));
        assert!(case.operator_propose(Outcome::Void, "too late".into(), 71 * S, &t, &n).is_err());
        assert!(case.dispute("too late", 71 * S).is_err());
    }

    #[test]
    fn only_a_proposal_can_be_disputed() {
        let mut case = Case::open(7, 2, 0);
        assert!(case.dispute("nothing to dispute", 0).is_err());
    }

    #[test]
    fn the_auto_approver_checks_before_it_signs() {
        assert_eq!(Case::approve(Outcome::State(1), &[Source::Alpha, Source::Beta], 2), Ok(APPROVERS.to_vec()));
        assert!(Case::approve(Outcome::State(2), &[Source::Alpha, Source::Beta], 2).is_err(), "no such state");
        assert!(Case::approve(Outcome::State(0), &[Source::Alpha], 2).is_err(), "one source");
        assert!(Case::approve(Outcome::Void, &[Source::Operator], 2).is_ok());
        assert!(Case::approve(Outcome::Void, &[Source::AutoApprover], 2).is_ok());
        assert!(Case::approve(Outcome::State(0), &[Source::AutoApprover], 2).is_err(), "it never picks a winner");
        // An operator proposing a state the event does not have is escalated rather than signed.
        let (t, n) = (timers(), names());
        let mut case = Case::open(7, 3, 0);
        case.operator_propose(Outcome::State(5), "typo".into(), 0, &t, &n).expect("accepted for review");
        assert!(matches!(case.stage, Stage::Escalated { ref reason, .. } if reason.contains("refused")));
    }

    #[test]
    fn late_reports_change_nothing_once_proposed() {
        let (t, n) = (timers(), names());
        let mut case = Case::open(7, 2, 0);
        case.report(report(Source::Alpha, Outcome::State(1), 0), &t, &n);
        case.report(report(Source::Beta, Outcome::State(1), 0), &t, &n);
        let before = proposal(&case);
        case.report(report(Source::Referee, Outcome::State(0), 5 * S), &t, &n);
        assert_eq!(proposal(&case), before);
    }

    #[test]
    fn a_resolution_from_elsewhere_closes_the_case() {
        let n = names();
        let mut case = Case::open(7, 3, 0);
        case.settled(Outcome::Void, 9 * S, &n);
        assert!(matches!(case.stage, Stage::Settled(ref p) if p.outcome == Outcome::Void && p.revision == 0));
        let shown = case.json(&n);
        assert_eq!(shown["stage"], "settled");
        assert_eq!(shown["detail"]["proposal"]["outcome"]["kind"], "void");
    }
}
