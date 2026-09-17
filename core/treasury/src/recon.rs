//! Reconciliation (blueprint §11): the treasury's ledger against the bank's statement, and the treasury's transfers
//! against what the sequencer group's journal says it applied, with the invariants that must hold between them.
//!
//! A run matches, resolves what it safely can and reports the rest as breaks for an operator:
//!
//! - **Bank.** A decision the statement shows and the treasury never heard of is booked as if its notice had come,
//!   with a posting memo that names the run. A transfer the treasury has at the bank and the bank has no record of is
//!   handed over again, which the bank takes once. A notice that arrives after its transfer moved on (a duplicate, or
//!   a late one reconciliation already booked) was ignored when it came, and is reported resolved. Everything else —
//!   outcomes that disagree, omnibus movements per transfer that differ, receipts for transfers the treasury never
//!   made — is reported, because the fix needs someone to find out which side is wrong.
//! - **Sequencer group.** A reclaim or grant sent without an answer is looked up by its command ID: if the journal
//!   has it, its outcome is applied as if the answer had come; if not, it is sent again under the same command ID,
//!   which the sequencer group deduplicates (spec E-7.1), so a send that did arrive after all moves nothing twice.
//!   A command the journal applied that the treasury does not expect, or one it expects that the journal (read past
//!   its sequence) does not have, or amounts that differ, are reported.
//! - **Invariants.** The ledger sums to zero; the bank's omnibus, less what it has decided and the treasury has not
//!   booked yet, equals the customers' cash plus money in transit (§11's "sum of customer cash equals the FBO balance
//!   minus in-flight transfers", with the bank's own balance rather than the ledger's copy of it); and for every
//!   account, what the treasury granted less what it reclaimed equals what the journal applied. A breach is a break
//!   like any other, and the run's status says `breach`. It does not halt anything: see the README.
//!
//! The journal is read from the node's feed, which replays it whole to each follower, so this is the nightly replay
//! check of §11 run as often as the schedule says. Runs are cheap here; the schedule is a flag.
//!
//! A break is found once: a later run that finds the same thing (kind, transfer, account) counts it as still open,
//! and a run that no longer finds it closes it. Resolving is idempotent, because each fix first checks that the
//! transfer is still where the break found it, and every fix a run makes goes into the book before it counts.

use crate::bank::{Bank, Outcome};
use crate::clock;
use crate::faults::Faults;
use crate::ledger;
use crate::transfer::{Kind, Reason, Stage, State as Standing, Transfer};
use crate::treasury::{command_id, locked, Adjusted, Command, Inner, Journal, Recorded, Refusal, Treasury};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

/// Runs kept for the operator surface. Breaks are all kept: they are the audit trail.
pub(crate) const RUNS: usize = 200;
/// How long a run the records could not take waits before it is tried again.
const POSTPONE_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    pub faults: Faults,
    /// How often a run starts on its own; 0 runs only when asked.
    pub every_ms: u64,
    /// How long after the bank decides a transfer its notice may still be on the way: a decision younger than this is
    /// in flight, not a break.
    pub grace_ms: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self { faults: Faults::NONE, every_ms: 60_000, grace_ms: 10_000 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Schedule,
    Operator,
}

/// What a run may do about a finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fix {
    Report,
    /// Book the bank's decision.
    Book {
        settles: bool,
        decided: u64,
    },
    /// Hand the transfer to the bank again.
    Rehand,
    /// Apply the outcome the journal holds for the command in doubt.
    Outcome(Adjusted),
    /// Send the command in doubt again, under the same command ID.
    Resend,
    /// Nothing to do: it was dealt with when it happened. Reported resolved.
    Noted(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub kind: String,
    pub transfer: Option<u64>,
    pub account: Option<u64>,
    pub detail: String,
    pub fix: Fix,
}

impl Finding {
    fn of(kind: &str, t: &Transfer, detail: String, fix: Fix) -> Self {
        Self { kind: kind.to_string(), transfer: Some(t.id), account: Some(t.account), detail, fix }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    pub at: u64,
    /// `reconciliation` or `operator`.
    pub by: String,
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Break {
    pub id: u64,
    pub kind: String,
    pub transfer: Option<u64>,
    pub account: Option<u64>,
    pub detail: String,
    pub found_at: u64,
    pub found_by: u64,
    pub resolution: Option<Resolution>,
}

impl Break {
    pub fn to_json(&self) -> Value {
        json!({
            "breakId": self.id.to_string(), "kind": self.kind, "transferId": self.transfer.map(|t| t.to_string()),
            "accountId": self.account.map(|a| a.to_string()), "detail": self.detail,
            "foundAt": clock::rfc3339(self.found_at), "foundByRun": self.found_by.to_string(),
            "state": if self.resolution.is_some() { "resolved" } else { "open" },
            "resolution": self.resolution.as_ref().map(|r| json!({"at": clock::rfc3339(r.at), "by": r.by, "note": r.note}))
        })
    }

    fn same(&self, f: &Finding) -> bool {
        self.kind == f.kind && self.transfer == f.transfer && self.account == f.account
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

pub struct State {
    pub breaks: BTreeMap<u64, Break>,
    pub runs: VecDeque<Value>,
    pub next_run: u64,
    pub due_at: u64,
    every_ms: u64,
    pub requested: bool,
    /// Notices that arrived for transfers already past the bank: which, and when.
    pub ignored: Vec<(u64, u64)>,
    /// No run starts before this: the last one could not reach the records.
    pub postponed_until: u64,
}

impl State {
    pub fn new(options: &Options, now: u64) -> Self {
        let mut state = Self {
            breaks: BTreeMap::new(),
            runs: VecDeque::new(),
            next_run: 1,
            due_at: u64::MAX,
            every_ms: options.every_ms,
            requested: false,
            ignored: Vec::new(),
            postponed_until: 0,
        };
        state.schedule(now);
        state
    }

    fn schedule(&mut self, now: u64) {
        self.due_at = if self.every_ms == 0 { u64::MAX } else { now.saturating_add(self.every_ms) };
    }

    /// Takes up the breaks and newest runs the records hold, and numbers the next run after every one of them.
    pub fn restore(&mut self, breaks: Vec<Break>, runs: Vec<Value>, next_run: u64) {
        self.breaks = breaks.into_iter().map(|b| (b.id, b)).collect();
        self.runs = runs.into_iter().collect();
        while self.runs.len() > RUNS {
            self.runs.pop_front();
        }
        self.next_run = self.next_run.max(next_run);
    }

    pub fn due(&self, now: u64) -> Option<Trigger> {
        if now < self.postponed_until {
            None
        } else if self.requested {
            Some(Trigger::Operator)
        } else if now >= self.due_at {
            Some(Trigger::Schedule)
        } else {
            None
        }
    }

    fn next_break(&self) -> u64 {
        self.breaks.keys().next_back().map_or(1, |b| b + 1)
    }
}

/// What the sequencer group should hold for one of a transfer's two commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    /// Sent or about to be: whatever the journal says is not a break yet.
    InFlight,
    Nothing,
    Applied,
    Rejected,
}

fn expect(t: &Transfer, what: char) -> Expect {
    match (what, t.kind, t.stage) {
        ('r', Kind::Deposit, _) => Expect::Nothing,
        ('r', Kind::Withdrawal, Stage::Reclaiming) => Expect::InFlight,
        ('r', Kind::Withdrawal, _) if t.reason == Some(Reason::InsufficientAvailableCash) => Expect::Rejected,
        ('r', Kind::Withdrawal, _) => Expect::Applied,
        (_, _, Stage::Granting { .. }) => Expect::InFlight,
        (_, Kind::Deposit, Stage::Done) if t.state == Standing::Settled => Expect::Applied,
        (_, Kind::Withdrawal, Stage::Done)
            if t.state == Standing::Cancelled || t.reason == Some(Reason::ReturnedByBank) =>
        {
            Expect::Applied
        }
        _ => Expect::Nothing,
    }
}

/// What the treasury believes the bank did with a transfer it has moved past the bank, if it went there at all.
fn believed(t: &Transfer) -> Option<Outcome> {
    match (t.stage, t.state, t.reason) {
        (Stage::Granting { then: Standing::Settled, .. }, _, _) | (Stage::Done, Standing::Settled, _) => {
            Some(Outcome::Settled)
        }
        (Stage::Granting { reason: Some(Reason::ReturnedByBank), .. }, _, _)
        | (Stage::Done, Standing::Failed, Some(Reason::DeclinedByBank | Reason::ReturnedByBank)) => {
            Some(Outcome::Failed)
        }
        (Stage::Granting { then: Standing::Cancelled, .. }, _, _) | (Stage::Done, Standing::Cancelled, _) => {
            Some(Outcome::Cancelled)
        }
        _ => None,
    }
}

/// Matches the treasury's transfers against the bank's record at `now`. Also returns how much the bank has moved the
/// omnibus for decisions still within the grace period, which the treasury is not expected to have booked yet.
pub fn match_bank(
    transfers: &BTreeMap<u64, Transfer>,
    bank: &Bank,
    fbo: &BTreeMap<u64, i64>,
    now: u64,
    grace_ms: u64,
) -> (Vec<Finding>, i64) {
    let mut findings = Vec::new();
    let mut in_flight = 0i64;
    for t in transfers.values() {
        let receipt = bank.receipt(t.id);
        match (t.stage, receipt) {
            (Stage::Reclaiming | Stage::ToBank, _) => {}
            (Stage::AtBank { .. }, None) => findings.push(Finding::of(
                "bank_no_record",
                t,
                format!("the treasury has transfer {} at the bank, and the bank has no record of it", t.id),
                Fix::Rehand,
            )),
            (Stage::AtBank { .. }, Some(r)) => match r.outcome(now) {
                Outcome::Pending => {}
                Outcome::Cancelled => findings.push(Finding::of(
                    "bank_outcome_mismatch",
                    t,
                    format!("the bank cancelled transfer {}, which the treasury still has at the bank", t.id),
                    Fix::Report,
                )),
                _ if r.decided.saturating_add(grace_ms) > now => in_flight += r.movement(now),
                outcome => findings.push(Finding::of(
                    "bank_decision_unbooked",
                    t,
                    format!(
                        "the bank's statement shows the {} of {} cents {} at {}, and no notice of it reached the treasury",
                        t.kind.name(),
                        t.amount,
                        outcome.name(),
                        clock::rfc3339(r.decided)
                    ),
                    Fix::Book { settles: outcome == Outcome::Settled, decided: r.decided },
                )),
            },
            (Stage::Granting { .. } | Stage::Done, _) => {
                let bank_says = receipt.map(|r| r.outcome(now));
                let agree = match (believed(t), bank_says) {
                    (None | Some(Outcome::Cancelled), None) => true,
                    (ours, theirs) => ours == theirs,
                };
                if !agree {
                    findings.push(Finding::of(
                        "bank_outcome_mismatch",
                        t,
                        format!(
                            "the treasury has transfer {} {}, and the bank {}",
                            t.id,
                            believed(t).map_or("never at the bank", Outcome::name),
                            bank_says.map_or_else(|| "has no record of it".to_string(), |o| format!("says {}", o.name()))
                        ),
                        Fix::Report,
                    ));
                    continue;
                }
                let ours = fbo.get(&t.id).copied().unwrap_or(0);
                let theirs = receipt.map_or(0, |r| r.movement(now));
                if ours != theirs {
                    findings.push(Finding::of(
                        "bank_amount_mismatch",
                        t,
                        format!(
                            "the ledger moved the omnibus by {ours} cents for transfer {}, and the bank by {theirs}",
                            t.id
                        ),
                        Fix::Report,
                    ));
                }
            }
        }
    }
    for r in bank.receipts().values().filter(|r| !transfers.contains_key(&r.transfer)) {
        findings.push(Finding {
            kind: "bank_unknown_transfer".into(),
            transfer: Some(r.transfer),
            account: Some(r.account),
            detail: format!("the bank has transfer {}, which the treasury never made", r.transfer),
            fix: Fix::Report,
        });
    }
    (findings, in_flight)
}

/// Matches the treasury's transfers against the treasury commands the journal holds.
pub fn match_engine(transfers: &BTreeMap<u64, Transfer>, journal: &Journal, nonce: u32) -> Vec<Finding> {
    let mut findings = Vec::new();
    for t in transfers.values() {
        for what in ['r', 'g'] {
            let id = command_id(nonce, t.id, what);
            let got = journal.commands.get(&id);
            let expected = expect(t, what);
            if expected == Expect::InFlight {
                if t.in_doubt.is_some() {
                    findings.push(match got {
                        Some(rec) => Finding::of(
                            "engine_answer_lost",
                            t,
                            format!("{id} was sent and its answer never read; the journal has it {:?}", rec.outcome),
                            Fix::Outcome(rec.outcome.clone()),
                        ),
                        None => Finding::of(
                            "engine_command_lost",
                            t,
                            format!("{id} was sent and never answered, and the journal does not have it"),
                            Fix::Resend,
                        ),
                    });
                }
                continue;
            }
            let reason = if what == 'r' { "RECLAIM" } else { "GRANT" };
            let mismatch = |detail: String| Finding::of("engine_outcome_mismatch", t, detail, Fix::Report);
            match (expected, got) {
                (Expect::Nothing, None) | (Expect::Rejected, Some(Recorded { outcome: Adjusted::Rejected(_), .. })) => {}
                (Expect::Nothing, Some(rec)) => findings.push(Finding::of(
                    "engine_unexpected_command",
                    t,
                    format!(
                        "the journal has {id} ({reason} of {} cents) {:?}, which transfer {} ({}) should never have sent",
                        rec.cents,
                        rec.outcome,
                        t.id,
                        t.state.name()
                    ),
                    Fix::Report,
                )),
                (Expect::Applied, None) => {
                    // The feed may simply not have reached it yet.
                    if t.sequence.is_some_and(|s| s <= journal.through) {
                        findings.push(Finding::of(
                            "engine_command_missing",
                            t,
                            format!(
                                "the treasury has {id} applied at sequence {}, and the journal through {} does not have it",
                                t.sequence.unwrap_or(0),
                                journal.through
                            ),
                            Fix::Report,
                        ));
                    }
                }
                (Expect::Rejected, None) => {}
                (Expect::Applied, Some(rec)) => match rec.outcome {
                    Adjusted::Applied(_) if rec.cents == t.amount && rec.account == t.account && rec.reason == reason => {}
                    Adjusted::Applied(_) => findings.push(Finding::of(
                        "engine_amount_mismatch",
                        t,
                        format!(
                            "the journal applied {id} as {} of {} cents to account {}; the treasury sent {reason} of {} \
                             cents to account {}",
                            rec.reason, rec.cents, rec.account, t.amount, t.account
                        ),
                        Fix::Report,
                    )),
                    ref other => {
                        findings.push(mismatch(format!("the treasury has {id} applied, and the journal {other:?}")));
                    }
                },
                (Expect::Rejected, Some(rec)) => {
                    findings.push(mismatch(format!("the treasury has {id} rejected, and the journal {:?}", rec.outcome)));
                }
                (Expect::InFlight, _) => {}
            }
        }
    }
    let prefix = format!("T{nonce:08x}");
    // Commands of earlier books on the same sequencer group are theirs, not breaks of this one.
    for (id, rec) in journal.commands.iter().filter(|(id, _)| id.starts_with(&prefix)) {
        let transfer = id.get(prefix.len()..prefix.len() + 6).and_then(|hex| u64::from_str_radix(hex, 16).ok());
        if transfer.is_none_or(|t| !transfers.contains_key(&t)) {
            findings.push(Finding {
                kind: "engine_unknown_command".into(),
                transfer,
                account: Some(rec.account),
                detail: format!(
                    "the journal has {id} ({} of {} cents) {:?}, for no transfer of this book",
                    rec.reason, rec.cents, rec.outcome
                ),
                fix: Fix::Report,
            });
        }
    }
    findings
}

/// Signed cents a command moves into a sleeve.
fn signed(reason: &str, cents: u64) -> i128 {
    let cents = i128::from(cents);
    if reason == "RECLAIM" {
        -cents
    } else {
        cents
    }
}

/// The invariants, over the treasury's state, the ledger of record (its balances and number of postings, as the
/// records sum them), the bank's record and the journal.
pub(crate) fn invariants(
    inner: &Inner,
    of_record: &(BTreeMap<String, i64>, u64),
    journal: &Journal,
    in_flight_at_bank: i64,
    now: u64,
) -> Vec<Check> {
    let (balances, postings) = of_record;
    let total: i128 = balances.values().map(|b| i128::from(*b)).sum();
    let nonzero = |m: &BTreeMap<String, i64>| {
        m.iter().filter(|(_, b)| **b != 0).map(|(a, b)| (a.clone(), *b)).collect::<Vec<_>>()
    };
    let agrees = nonzero(balances) == nonzero(inner.ledger.balances()) && *postings == inner.ledger.postings();
    let mut checks = vec![Check {
        name: "ledger_sums_to_zero",
        ok: total == 0 && agrees,
        detail: if agrees {
            format!("the ledger of record sums to {total} cents over {postings} postings")
        } else {
            format!(
                "the ledger of record sums to {total} cents over {postings} postings, and the treasury's working copy \
                 ({} postings, summing to {} cents) differs from it",
                inner.ledger.postings(),
                inner.ledger.total()
            )
        },
    }];

    let balances = inner.ledger.balances();
    let customers: i128 =
        balances.iter().filter(|(a, _)| a.starts_with("customer:")).map(|(_, c)| i128::from(*c)).sum();
    let transit = i128::from(inner.ledger.balance(ledger::DEPOSITS_IN_TRANSIT))
        + i128::from(inner.ledger.balance(ledger::WITHDRAWALS_IN_TRANSIT));
    let bank = i128::from(inner.bank.balance(now));
    let booked = bank - i128::from(in_flight_at_bank);
    // Customer cash and money in transit are credit balances; what backs them is the omnibus, a debit.
    let owed = -(customers + transit);
    checks.push(Check {
        name: "omnibus_covers_customer_cash",
        ok: booked == owed,
        detail: format!(
            "the bank holds {bank} cents, {in_flight_at_bank} of it decided and not yet booked; customers' cash is {} \
             and {} is in transit",
            -customers, -transit
        ),
    });

    let mut treasury: BTreeMap<u64, i128> = BTreeMap::new();
    let mut engine: BTreeMap<u64, i128> = BTreeMap::new();
    for t in inner.transfers.values() {
        if matches!(t.stage, Stage::Reclaiming | Stage::Granting { .. })
            || t.sequence.is_some_and(|s| s > journal.through)
        {
            continue;
        }
        for (what, reason) in [('r', "RECLAIM"), ('g', "GRANT")] {
            if expect(t, what) == Expect::Applied {
                *treasury.entry(t.account).or_default() += signed(reason, t.amount);
            }
            if let Some(rec) = journal.commands.get(&command_id(inner.nonce, t.id, what)) {
                if matches!(rec.outcome, Adjusted::Applied(_)) {
                    *engine.entry(rec.account).or_default() += signed(&rec.reason, rec.cents);
                }
            }
        }
    }
    treasury.retain(|_, c| *c != 0);
    engine.retain(|_, c| *c != 0);
    let differ: Vec<String> = treasury
        .keys()
        .chain(engine.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|a| treasury.get(a) != engine.get(a))
        .map(|a| {
            format!(
                "account {a}: treasury {} cents, journal {}",
                treasury.get(a).copied().unwrap_or(0),
                engine.get(a).copied().unwrap_or(0)
            )
        })
        .collect();
    checks.push(Check {
        name: "journal_matches_treasury",
        ok: differ.is_empty(),
        detail: if differ.is_empty() {
            format!("net grants agree for {} accounts, through sequence {}", treasury.len(), journal.through)
        } else {
            differ.join("; ")
        },
    });
    checks
}

fn check_json(c: &Check) -> Value {
    json!({"name": c.name, "ok": c.ok, "detail": c.detail})
}

impl Treasury {
    /// One reconciliation run, start to finish, under the lock: everything it reads is in memory but the ledger of
    /// record's sums, and every fix it makes is written to the records. A run that cannot reach the records is not
    /// started, and is tried again a few seconds later; says whether it ran.
    pub(crate) fn reconcile(&self, inner: &mut Inner, journal: &Journal, trigger: Trigger, now: u64) -> bool {
        match self.fresh(inner).and_then(|()| self.records.ping().map_err(String::from)) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("treasury: reconciliation postponed, the records are unavailable: {e}");
                inner.recon.postponed_until = now.saturating_add(POSTPONE_MS);
                return false;
            }
        }
        let options = self.config.recon;
        let run = inner.recon.next_run;
        inner.recon.next_run += 1;
        inner.recon.requested = false;
        inner.recon.schedule(now);
        // Every break this run found or found again, and those it resolved.
        let mut seen: Vec<(u64, bool)> = Vec::new();
        let mut resolved: Vec<u64> = Vec::new();

        let (findings, _) = match_bank(&inner.transfers, &inner.bank, &inner.fbo, now, options.grace_ms);
        for f in findings {
            seen.push(self.take_up(inner, run, f, now, &mut resolved));
        }
        for (transfer, at) in std::mem::take(&mut inner.recon.ignored) {
            let booked =
                inner.recon.breaks.values().any(|b| {
                    b.kind == "bank_decision_unbooked" && b.transfer == Some(transfer) && b.resolution.is_some()
                });
            let account = inner.transfers.get(&transfer).map(|t| t.account);
            let (kind, detail) = if booked {
                (
                    "bank_late_notice",
                    format!(
                        "the bank's notice of transfer {transfer} arrived at {}, after reconciliation had booked it",
                        clock::rfc3339(at)
                    ),
                )
            } else {
                (
                    "bank_duplicate_notice",
                    format!("a second notice of transfer {transfer} arrived at {}", clock::rfc3339(at)),
                )
            };
            let note = "ignored when it arrived: the transfer was already past the bank, and its omnibus movement \
                        matches the bank's statement unless a bank_amount_mismatch says otherwise";
            let f =
                Finding { kind: kind.into(), transfer: Some(transfer), account, detail, fix: Fix::Noted(note.into()) };
            // Each notice is its own event, so it is never merged into an earlier break.
            seen.push((self.record_new(inner, run, f, now, &mut resolved), true));
        }

        for f in match_engine(&inner.transfers, journal, inner.nonce) {
            seen.push(self.take_up(inner, run, f, now, &mut resolved));
        }

        let (_, in_flight) = match_bank(&inner.transfers, &inner.bank, &inner.fbo, now, options.grace_ms);
        // The ledger of record is summed after the run's fixes are written, so it includes them. Should the records
        // stop answering in between, the working copy is checked on its own, and the check says so.
        let of_record = self.records.trial_balance().map_err(|e| {
            eprintln!("treasury: reconciliation run {run} could not sum the ledger of record: {e}");
        });
        let of_record = of_record.unwrap_or_else(|()| (inner.ledger.balances().clone(), inner.ledger.postings()));
        let checks = invariants(inner, &of_record, journal, in_flight, now);
        for c in checks.iter().filter(|c| !c.ok) {
            let f = Finding {
                kind: format!("invariant_{}", c.name),
                transfer: None,
                account: None,
                detail: c.detail.clone(),
                fix: Fix::Report,
            };
            seen.push(self.take_up(inner, run, f, now, &mut resolved));
        }

        // What an earlier run found and this one did not is no longer so.
        let gone: Vec<u64> = inner
            .recon
            .breaks
            .values()
            .filter(|b| b.resolution.is_none() && !seen.iter().any(|(id, _)| *id == b.id))
            .map(|b| b.id)
            .collect();
        for id in gone {
            let note = format!("not found again by run {run}");
            if self.close(inner, id, "reconciliation", &note, now).is_ok() {
                resolved.push(id);
            }
        }

        let found: Vec<u64> = seen.iter().filter(|(_, new)| *new).map(|(id, _)| *id).collect();
        let open: Vec<u64> = inner.recon.breaks.values().filter(|b| b.resolution.is_none()).map(|b| b.id).collect();
        let status = if checks.iter().any(|c| !c.ok) {
            "breach"
        } else if !open.is_empty() {
            "breaks"
        } else if !resolved.is_empty() {
            "resolved"
        } else {
            "clean"
        };
        let statement = inner.bank.statement(0, now);
        let ids = |v: &[u64]| v.iter().map(u64::to_string).collect::<Vec<_>>();
        let report = json!({
            "runId": run.to_string(),
            "trigger": match trigger { Trigger::Schedule => "schedule", Trigger::Operator => "operator" },
            "at": clock::rfc3339(now),
            "status": status,
            "bank": {
                "statementTo": clock::rfc3339(now), "openingCents": statement.opening, "closingCents": statement.closing,
                "lines": statement.lines.len(), "inFlightCents": in_flight
            },
            "journal": {"commands": journal.commands.len(), "through": journal.through.to_string()},
            "found": ids(&found), "resolved": ids(&resolved), "open": ids(&open),
            "invariants": checks.iter().map(check_json).collect::<Vec<_>>()
        });
        // Every run goes into the records, so run numbers never repeat after a restart and each note that names a run
        // names one that can be read back.
        if let Err(e) = self.records.record_run(run, status, &report, now) {
            inner.stale = true;
            eprintln!("treasury: recording run {run}: {e}");
        }
        if status != "clean" {
            eprintln!(
                "treasury: reconciliation run {run}: {status}, {} found, {} resolved, {} open{}",
                found.len(),
                resolved.len(),
                open.len(),
                checks
                    .iter()
                    .filter(|c| !c.ok)
                    .map(|c| format!("; INVARIANT BREACHED {}: {}", c.name, c.detail))
                    .collect::<String>()
            );
        }
        inner.recon.runs.push_back(report);
        while inner.recon.runs.len() > RUNS {
            inner.recon.runs.pop_front();
        }
        inner.generation += 1;
        self.changed.notify_all();
        true
    }

    /// Takes up a finding: the open break it already is, or a new one; and fixes it if it can. Returns the break and
    /// whether it is new.
    fn take_up(&self, inner: &mut Inner, run: u64, f: Finding, now: u64, resolved: &mut Vec<u64>) -> (u64, bool) {
        let open = inner.recon.breaks.values().find(|b| b.resolution.is_none() && b.same(&f)).map(|b| b.id);
        match open {
            Some(id) => {
                if f.fix != Fix::Report && self.fix(inner, run, id, &f, now) {
                    resolved.push(id);
                }
                (id, false)
            }
            None => (self.record_new(inner, run, f, now, resolved), true),
        }
    }

    fn record_new(&self, inner: &mut Inner, run: u64, f: Finding, now: u64, resolved: &mut Vec<u64>) -> u64 {
        let id = inner.recon.next_break();
        let b = Break {
            id,
            kind: f.kind.clone(),
            transfer: f.transfer,
            account: f.account,
            detail: f.detail.clone(),
            found_at: now,
            found_by: run,
            resolution: None,
        };
        if let Err(e) = self.records.record_break(&b) {
            inner.stale = true;
            eprintln!("treasury: recording break {id}: {e}");
        }
        inner.recon.breaks.insert(id, b);
        if f.fix != Fix::Report && self.fix(inner, run, id, &f, now) {
            resolved.push(id);
        }
        id
    }

    /// Applies a finding's fix and, if it worked, resolves its break with a note of what was done. Says whether it
    /// did.
    fn fix(&self, inner: &mut Inner, run: u64, id: u64, f: &Finding, now: u64) -> bool {
        match self.apply_fix(inner, run, id, f, now) {
            Ok(note) => self.close(inner, id, "reconciliation", &note, now).is_ok(),
            Err(e) => {
                eprintln!("treasury: reconciliation run {run} could not resolve break {id}: {e}");
                false
            }
        }
    }

    fn apply_fix(&self, inner: &mut Inner, run: u64, id: u64, f: &Finding, now: u64) -> Result<String, String> {
        let transfer = f.transfer.and_then(|t| inner.transfers.get(&t)).cloned();
        let mut t = match (&f.fix, transfer) {
            (Fix::Noted(note), _) => return Ok(note.clone()),
            (Fix::Report, _) => return Err("nothing to do but report it".into()),
            (_, None) => return Err("the transfer is gone".into()),
            (_, Some(t)) => t,
        };
        match &f.fix {
            Fix::Book { settles, decided } => {
                if !matches!(t.stage, Stage::AtBank { .. }) {
                    return Err(format!("transfer {} is no longer at the bank", t.id));
                }
                let posting = Self::decide(&mut t, *settles)?;
                let way = if *settles { "settled" } else { "failed" };
                let memo = format!(
                    "{}, booked by reconciliation run {run} (break {id}): the bank's statement shows it {way} at {}, \
                     and no notice of it arrived",
                    posting.as_ref().map_or(t.kind.name(), |(m, _)| m),
                    clock::rfc3339(*decided)
                );
                let postings: Vec<_> = posting.into_iter().map(|(_, p)| (memo.as_str(), p)).collect();
                self.commit(inner, t, &postings, now)?;
                Ok(format!("booked as {way} from the bank's statement"))
            }
            Fix::Rehand => {
                if !matches!(t.stage, Stage::AtBank { .. }) {
                    return Err(format!("transfer {} is no longer at the bank", t.id));
                }
                t.stage = Stage::ToBank;
                self.commit(inner, t, &[], now)?;
                Ok("handed to the bank again".into())
            }
            Fix::Outcome(outcome) => {
                let command = Command::for_transfer(inner.nonce, &t)
                    .filter(|_| t.in_doubt.is_some())
                    .ok_or_else(|| format!("transfer {} is no longer waiting on an answer", t.id))?;
                let sequence = match outcome {
                    Adjusted::Applied(s) => format!("applied at sequence {s}"),
                    other => format!("{other:?}"),
                };
                let command_id = command.command_id.clone();
                self.apply(inner, &command, Ok(outcome.clone()), now)?;
                Ok(format!("outcome of {command_id} read from the journal by command ID: {sequence}"))
            }
            Fix::Resend => {
                let command = Command::for_transfer(inner.nonce, &t)
                    .filter(|_| t.in_doubt.is_some())
                    .ok_or_else(|| format!("transfer {} is no longer waiting on an answer", t.id))?;
                t.in_doubt = None;
                inner.retry_at.remove(&t.id);
                self.commit(inner, t, &[], now)?;
                Ok(format!(
                    "sent again under the same command ID {}, which the sequencer group deduplicates",
                    command.command_id
                ))
            }
            Fix::Report | Fix::Noted(_) => Err("nothing to do".into()),
        }
    }

    fn close(&self, inner: &mut Inner, id: u64, by: &str, note: &str, now: u64) -> Result<Value, Refusal> {
        let b = inner.recon.breaks.get(&id).ok_or_else(|| Refusal::not_found("no such break"))?;
        if b.resolution.is_some() {
            return Err(Refusal::new(409, "BREAK_ALREADY_RESOLVED", format!("break {id} is already resolved")));
        }
        let mut b = b.clone();
        b.resolution = Some(Resolution { at: now, by: by.to_string(), note: note.to_string() });
        self.records.record_break(&b).map_err(|e| {
            inner.stale = true;
            eprintln!("treasury: {e}");
            Refusal::unavailable("the treasury could not record the resolution; nothing was done")
        })?;
        let body = b.to_json();
        inner.recon.breaks.insert(id, b);
        inner.generation += 1;
        self.changed.notify_all();
        Ok(body)
    }

    /// The operator's summary: the schedule, what is being injected, the last run and every open break.
    pub fn reconciliation(&self) -> Value {
        let inner = locked(&self.inner);
        let options = self.config.recon;
        let open: Vec<Value> =
            inner.recon.breaks.values().filter(|b| b.resolution.is_none()).map(Break::to_json).collect();
        let recent: Vec<Value> =
            inner.recon.breaks.values().rev().filter(|b| b.resolution.is_some()).take(20).map(Break::to_json).collect();
        json!({
            "everyMs": options.every_ms, "graceMs": options.grace_ms,
            "nextRunAt": (inner.recon.due_at != u64::MAX).then(|| clock::rfc3339(inner.recon.due_at)),
            "faults": options.faults.to_json(), "faultsOn": options.faults.any(),
            "lastRun": inner.recon.runs.back(), "openBreaks": open, "recentlyResolved": recent,
            "breaks": inner.recon.breaks.len()
        })
    }

    pub fn runs(&self, limit: usize) -> Value {
        let inner = locked(&self.inner);
        json!({"runs": inner.recon.runs.iter().rev().take(limit).collect::<Vec<_>>()})
    }

    /// Breaks newest first: `open`, `resolved`, or all of them.
    pub fn breaks(&self, state: Option<&str>, limit: usize) -> Result<Value, Refusal> {
        let want = match state {
            None | Some("all") => None,
            Some("open") => Some(false),
            Some("resolved") => Some(true),
            Some(other) => return Err(Refusal::bad(format!("state must be open, resolved or all, not {other}"))),
        };
        let inner = locked(&self.inner);
        let breaks: Vec<Value> = inner
            .recon
            .breaks
            .values()
            .rev()
            .filter(|b| want.is_none_or(|w| b.resolution.is_some() == w))
            .take(limit)
            .map(Break::to_json)
            .collect();
        Ok(json!({"breaks": breaks}))
    }

    /// Asks the driver for a run now, and waits up to `limit` for it to finish.
    pub fn request_run(&self, limit: Duration) -> Value {
        let mut inner = locked(&self.inner);
        let before = inner.recon.next_run;
        inner.recon.requested = true;
        inner.generation += 1;
        self.changed.notify_all();
        let (inner, _) = self
            .changed
            .wait_timeout_while(inner, limit, |i| i.recon.next_run == before)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.recon.next_run == before {
            json!({"requested": true, "runId": before.to_string()})
        } else {
            inner.recon.runs.back().cloned().unwrap_or(Value::Null)
        }
    }

    /// An operator marks a break resolved, saying how.
    pub fn resolve_break(&self, id: u64, body: &Value, now: u64) -> Result<Value, Refusal> {
        let note = body["note"].as_str().map(str::trim).unwrap_or("");
        if !(1..=1000).contains(&note.len()) {
            return Err(Refusal::bad("note is required: 1 to 1000 characters saying how the break was resolved"));
        }
        let mut inner = locked(&self.inner);
        self.fresh(&mut inner).map_err(|e| {
            eprintln!("treasury: {e}");
            Refusal::unavailable("the treasury's records are unavailable; nothing was done")
        })?;
        self.close(&mut inner, id, "operator", note, now)
    }

    /// The fake bank's statement of the omnibus for a period, in wall-clock milliseconds.
    pub fn statement(&self, from: u64, to: u64) -> Value {
        locked(&self.inner).bank.statement(from, to).to_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::faults::{Faults, Lossy};
    use crate::ledger::{customer, FBO};
    use crate::records::Memory;
    use crate::treasury::tests::{balanced, body, open_with, state, Group, ACCOUNT};
    use crate::treasury::Sequencer;

    fn options(faults: Faults) -> Options {
        Options { faults, every_ms: 0, grace_ms: 500 }
    }

    /// Steps the treasury every 50 ms from `from` to `to`.
    fn advance(treasury: &Treasury, group: &mut dyn Sequencer, from: u64, to: u64) {
        for now in (from..=to).step_by(50) {
            treasury.step(group, now);
        }
    }

    /// Runs reconciliation now, as an operator would ask for it, and returns the run.
    fn reconcile(treasury: &Treasury, group: &mut dyn Sequencer, now: u64) -> Value {
        locked(&treasury.inner).recon.requested = true;
        treasury.step(group, now);
        locked(&treasury.inner).recon.runs.back().cloned().expect("a run")
    }

    fn kinds(treasury: &Treasury, state: &str) -> Vec<String> {
        let breaks = treasury.breaks(Some(state), 500).expect("breaks");
        let mut kinds: Vec<String> = breaks["breaks"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|b| b["kind"].as_str().unwrap_or("").into())
            .collect();
        kinds.sort();
        kinds
    }

    fn postings(treasury: &Treasury) -> Value {
        treasury.trial_balance()["postings"].clone()
    }

    fn invariants_hold(run: &Value) {
        for check in run["invariants"].as_array().into_iter().flatten() {
            assert_eq!(check["ok"], true, "{check}");
        }
    }

    #[test]
    fn a_lost_notice_is_booked_from_the_statement_once() {
        let p = Memory::default();
        let faults = Faults { lost_notice_per_mille: 1000, ..Faults::NONE };
        let treasury = open_with(&p, options(faults));
        let mut group = Group::default();
        let id = treasury.request(ACCOUNT, &body("d", "deposit", 700, "dummy_bank"), 1_000).expect("requested");
        advance(&treasury, &mut group, 1_000, 3_000);
        assert_eq!(state(&treasury, id).0, "pending", "the notice was lost, so nothing moved");
        assert!(group.sleeves.is_empty());

        let run = reconcile(&treasury, &mut group, 3_000);
        assert_eq!(run["status"], "resolved", "{run}");
        assert_eq!(kinds(&treasury, "resolved"), vec!["bank_decision_unbooked"]);
        invariants_hold(&run);
        advance(&treasury, &mut group, 3_000, 3_100);
        assert_eq!(state(&treasury, id).0, "settled");
        assert_eq!(group.sleeves[&ACCOUNT], 700);
        balanced(&treasury);
        assert_eq!(treasury.balance(FBO), 50_700);

        // Found again, it is not booked again.
        let before = postings(&treasury);
        let again = reconcile(&treasury, &mut group, 4_000);
        assert_eq!(again["status"], "clean", "{again}");
        assert_eq!(postings(&treasury), before);
        assert_eq!(group.sleeves[&ACCOUNT], 700);
    }

    #[test]
    fn a_decision_inside_the_grace_period_is_in_flight_not_a_break() {
        let p = Memory::default();
        let faults = Faults { lost_notice_per_mille: 1000, ..Faults::NONE };
        let treasury = open_with(&p, options(faults));
        let mut group = Group::default();
        treasury.request(ACCOUNT, &body("d", "deposit", 700, "dummy_instant"), 1_000).expect("requested");
        treasury.step(&mut group, 1_000);
        let run = reconcile(&treasury, &mut group, 1_100);
        assert_eq!(run["status"], "clean", "{run}");
        assert_eq!(run["bank"]["inFlightCents"], 700);
        invariants_hold(&run);
    }

    #[test]
    fn a_late_notice_after_booking_and_a_duplicate_notice_are_ignored_and_reported() {
        let p = Memory::default();
        let faults = Faults { late_notice_per_mille: 1000, late_notice_ms: 5_000, ..Faults::NONE };
        let treasury = open_with(&p, options(faults));
        let mut group = Group::default();
        group.sleeves.insert(ACCOUNT, 1_000);
        let w = treasury.request(ACCOUNT, &body("w", "withdrawal", 400, "dummy_bank"), 1_000).expect("requested");
        advance(&treasury, &mut group, 1_000, 2_000);
        assert_eq!(state(&treasury, w).0, "pending");
        reconcile(&treasury, &mut group, 2_000);
        assert_eq!(state(&treasury, w).0, "settled");
        let before = postings(&treasury);
        advance(&treasury, &mut group, 2_000, 8_000);
        assert_eq!(postings(&treasury), before, "the late notice was acted on");
        let run = reconcile(&treasury, &mut group, 8_000);
        assert_eq!(kinds(&treasury, "resolved"), vec!["bank_decision_unbooked", "bank_late_notice"], "{run}");
        invariants_hold(&run);
        balanced(&treasury);

        let p = Memory::default();
        let faults = Faults { duplicate_notice_per_mille: 1000, ..Faults::NONE };
        let treasury = open_with(&p, options(faults));
        let d = treasury.request(ACCOUNT, &body("d", "deposit", 300, "dummy_bank"), 1_000).expect("requested");
        advance(&treasury, &mut group, 1_000, 5_000);
        assert_eq!(state(&treasury, d).0, "settled");
        assert_eq!(treasury.balance(&customer(ACCOUNT)), -50_300, "posted twice");
        let run = reconcile(&treasury, &mut group, 5_000);
        assert_eq!(run["status"], "resolved", "{run} {}", treasury.breaks(None, 10).expect("breaks"));
        assert_eq!(kinds(&treasury, "resolved"), vec!["bank_duplicate_notice"]);
        invariants_hold(&run);
    }

    #[test]
    fn a_posting_the_bank_never_made_is_a_break_and_a_breach_for_an_operator() {
        let p = Memory::default();
        let treasury = open_with(&p, options(Faults::NONE));
        let mut group = Group::default();
        let d = treasury.request(ACCOUNT, &body("d", "deposit", 300, "dummy_instant"), 1_000).expect("requested");
        advance(&treasury, &mut group, 1_000, 1_100);
        // As a treasury that acted on a duplicate notice would: the deposit received twice.
        {
            let mut inner = locked(&treasury.inner);
            let t = inner.transfers[&d].clone();
            let posting = crate::ledger::Posting::moving(300, FBO, &customer(ACCOUNT)).expect("a posting");
            treasury.commit(&mut inner, t, &[("deposit received at the bank", posting)], 1_100).expect("committed");
        }
        let run = reconcile(&treasury, &mut group, 2_000);
        assert_eq!(run["status"], "breach", "{run}");
        assert_eq!(kinds(&treasury, "open"), vec!["bank_amount_mismatch", "invariant_omnibus_covers_customer_cash"]);
        let broken: Vec<&Value> =
            run["invariants"].as_array().into_iter().flatten().filter(|c| c["ok"] == false).collect();
        assert_eq!(broken.len(), 1, "{run}");

        // The same thing found again is the same break.
        let again = reconcile(&treasury, &mut group, 3_000);
        assert_eq!(again["found"], json!([]), "{again}");
        assert_eq!(kinds(&treasury, "open").len(), 2);

        // An operator resolves one with a note, once.
        let open = treasury.breaks(Some("open"), 10).expect("breaks");
        let id: u64 = open["breaks"][0]["breakId"].as_str().and_then(|b| b.parse().ok()).expect("an id");
        assert_eq!(treasury.resolve_break(id, &json!({}), 3_100).map_err(|r| r.status), Err(400));
        let resolved = treasury.resolve_break(id, &json!({"note": "reversed by hand"}), 3_100).expect("resolved");
        assert_eq!(resolved["resolution"]["by"], "operator");
        let twice = treasury.resolve_break(id, &json!({"note": "again"}), 3_200).map_err(|r| (r.status, r.code));
        assert_eq!(twice, Err((409, "BREAK_ALREADY_RESOLVED")));
        assert_eq!(treasury.resolve_break(999, &json!({"note": "x"}), 3_200).map_err(|r| r.status), Err(404));
    }

    #[test]
    fn a_lost_answer_is_looked_up_and_a_lost_command_is_sent_again_once() {
        let p = Memory::default();
        let faults = Faults { lost_answer_per_mille: 1000, ..Faults::NONE };
        let treasury = open_with(&p, options(faults));
        let mut lossy = Lossy::new(Group::default(), faults, 5);
        let d = treasury.request(ACCOUNT, &body("d", "deposit", 900, "dummy_instant"), 1_000).expect("requested");
        advance(&treasury, &mut lossy, 1_000, 3_000);
        assert_eq!(state(&treasury, d).0, "pending", "the answer was lost");
        assert_eq!(lossy.inner().sleeves[&ACCOUNT], 900, "applied all the same");
        assert_eq!(lossy.inner().sent, 1, "sent again without reconciliation");
        let run = reconcile(&treasury, &mut lossy, 3_000);
        assert_eq!(kinds(&treasury, "resolved"), vec!["engine_answer_lost"], "{run}");
        invariants_hold(&run);
        assert_eq!(state(&treasury, d).0, "settled");
        assert_eq!(lossy.inner().sent, 1, "looked up, not sent again");
        assert_eq!(reconcile(&treasury, &mut lossy, 4_000)["status"], "clean");
        balanced(&treasury);

        let p = Memory::default();
        let faults = Faults { lost_command_per_mille: 1000, ..Faults::NONE };
        let treasury = open_with(&p, options(faults));
        let mut group = Group::default();
        group.sleeves.insert(ACCOUNT, 1_000);
        let mut lossy = Lossy::new(group, faults, 5);
        let w = treasury.request(ACCOUNT, &body("w", "withdrawal", 600, "dummy_instant"), 1_000).expect("requested");
        advance(&treasury, &mut lossy, 1_000, 3_000);
        assert_eq!(state(&treasury, w).0, "pending");
        assert_eq!((lossy.inner().sleeves[&ACCOUNT], lossy.inner().sent), (1_000, 0), "the reclaim never arrived");
        let run = reconcile(&treasury, &mut lossy, 3_000);
        assert_eq!(kinds(&treasury, "resolved"), vec!["engine_command_lost"], "{run}");
        advance(&treasury, &mut lossy, 3_000, 3_200);
        assert_eq!(state(&treasury, w).0, "settled");
        assert_eq!((lossy.inner().sleeves[&ACCOUNT], lossy.inner().sent), (400, 1), "reclaimed once");
        invariants_hold(&reconcile(&treasury, &mut lossy, 4_000));
        balanced(&treasury);
    }

    #[test]
    fn a_command_the_treasury_never_sent_is_reported_with_a_breach() {
        let p = Memory::default();
        let treasury = open_with(&p, options(Faults::NONE));
        let mut group = Group::default();
        let d = treasury.request(ACCOUNT, &body("d", "deposit", 250, "dummy_failing"), 1_000).expect("requested");
        advance(&treasury, &mut group, 1_000, 1_500);
        assert_eq!(state(&treasury, d).0, "failed");
        // Something granted the failed deposit anyway, under its command ID.
        let id = command_id(locked(&treasury.inner).nonce, d, 'g');
        group.adjust(&id, ACCOUNT, 250, "GRANT").expect("applied");
        let run = reconcile(&treasury, &mut group, 2_000);
        assert_eq!(run["status"], "breach", "{run}");
        assert_eq!(kinds(&treasury, "open"), vec!["engine_unexpected_command", "invariant_journal_matches_treasury"]);
    }

    #[test]
    fn with_every_loss_injected_every_transfer_finishes_and_every_run_keeps_the_invariants() {
        let p = Memory::default();
        let faults = Faults {
            lost_notice_per_mille: 150,
            late_notice_per_mille: 150,
            late_notice_ms: 3_000,
            duplicate_notice_per_mille: 150,
            lost_answer_per_mille: 150,
            lost_command_per_mille: 150,
        };
        let treasury = open_with(&p, Options { faults, every_ms: 1_000, grace_ms: 500 });
        let mut group = Group::default();
        group.sleeves.insert(ACCOUNT, 50_000);
        let mut lossy = Lossy::new(group, faults, 5);
        let mut ids = Vec::new();
        for n in 0..80u64 {
            let now = 1_000 + n * 50;
            let (kind, method) = match n % 5 {
                0 | 1 => ("deposit", "dummy_bank"),
                2 => ("withdrawal", "dummy_bank"),
                3 => ("deposit", "dummy_instant"),
                _ => ("withdrawal", "dummy_failing"),
            };
            let id = treasury.request(ACCOUNT, &body(&format!("t{n}"), kind, 100 + n, method), now).expect("requested");
            if n % 11 == 0 {
                let _ = treasury.cancel(ACCOUNT, id, now);
            }
            ids.push(id);
            treasury.step(&mut lossy, now);
        }
        let mut runs = 0;
        for now in (5_000..=40_000).step_by(50) {
            treasury.step(&mut lossy, now);
            let run = locked(&treasury.inner).recon.runs.back().cloned();
            if let Some(run) = run.filter(|r| r["at"] == clock::rfc3339(now)) {
                invariants_hold(&run);
                runs += 1;
            }
        }
        assert!(runs > 30, "{runs} runs");
        for id in &ids {
            assert_ne!(state(&treasury, *id).0, "pending", "transfer {id} never finished");
        }
        let run = reconcile(&treasury, &mut lossy, 40_000);
        invariants_hold(&run);
        assert_eq!(kinds(&treasury, "open"), Vec::<String>::new(), "{run}");
        let found = kinds(&treasury, "resolved");
        for kind in [
            "bank_decision_unbooked",
            "bank_late_notice",
            "bank_duplicate_notice",
            "engine_answer_lost",
            "engine_command_lost",
        ] {
            assert!(found.iter().any(|k| k == kind), "no {kind} among {found:?}");
        }
        // The sleeve holds the seed plus what the treasury says it granted less what it reclaimed, each once.
        let moved: i64 = ids
            .iter()
            .map(|id| treasury.get(ACCOUNT, *id).expect("a transfer"))
            .filter(|t| t["state"] == "settled")
            .map(|t| {
                let cents = t["amountCents"].as_i64().unwrap_or(0);
                if t["kind"] == "deposit" {
                    cents
                } else {
                    -cents
                }
            })
            .sum();
        assert_eq!(lossy.inner().sleeves[&ACCOUNT], 50_000 + moved);
        balanced(&treasury);

        // Breaks and runs worth keeping survive a restart, and nothing is found twice.
        let before = treasury.breaks(None, 500).expect("breaks");
        drop(treasury);
        let treasury = open_with(&p, Options { faults, every_ms: 0, grace_ms: 500 });
        assert_eq!(treasury.breaks(None, 500).expect("breaks"), before);
        let run = reconcile(&treasury, &mut lossy, 41_000);
        assert_eq!(run["found"], json!([]), "{run}");
        invariants_hold(&run);
    }
}
