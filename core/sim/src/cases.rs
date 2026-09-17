//! The dummy oracle's records in PostgreSQL (`migrations/`): resolution cases, with every source report, proposal,
//! sign-off, dispute and line of history, and the postponements and suspensions operators made by hand. The oracle
//! writes a case whenever it changes, before acting on the change, so a restarted oracle carries on each case where it
//! was, manual actions included; the calendar's decisions still come from the journal.

use crate::resolution::{Case, Outcome, Proposal, Report, Source, Stage, APPROVERS};
use std::collections::{BTreeMap, BTreeSet};
use venue_store::postgres::{Row, Transaction};
use venue_store::{big, unsigned, Database, Error, Migration};

/// The oracle's schema, in order. A released migration is never edited: a change is a new one.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "resolution_cases",
    sql: include_str!("../migrations/V001__resolution_cases.sql"),
}];

/// What the oracle holds that the journal does not, as the records have it.
#[derive(Debug, Default, PartialEq)]
pub struct Held {
    pub cases: Vec<Case>,
    /// Postponed rounds, until when.
    pub holds: BTreeMap<u64, u64>,
    pub suspended_by_hand: BTreeSet<u64>,
}

#[derive(Clone)]
pub struct CaseStore {
    db: Database,
}

fn outcome_columns(outcome: Outcome) -> (&'static str, Option<i64>) {
    match outcome {
        Outcome::State(s) => ("state", i64::try_from(s).ok()),
        Outcome::Void => ("void", None),
    }
}

fn outcome_from(kind: &str, state: Option<i64>) -> Result<Outcome, Error> {
    match (kind, state) {
        ("void", _) => Ok(Outcome::Void),
        ("state", Some(s)) => Ok(Outcome::State(unsigned(s)?)),
        _ => Err(Error::refused(format!("an outcome {kind} {state:?}"))),
    }
}

fn source(name: &str) -> Result<Source, Error> {
    Source::parse(name).ok_or_else(|| Error::refused(format!("an unknown source {name}")))
}

fn proposal_from(row: &Row, signatures: Vec<&'static str>) -> Result<Proposal, Error> {
    let sources: Vec<String> = row.get("sources");
    Ok(Proposal {
        outcome: outcome_from(row.get::<_, &str>("outcome"), row.get("winning_state"))?,
        sources: sources.iter().map(|s| source(s)).collect::<Result<_, _>>()?,
        evidence: row.get("evidence"),
        signatures,
        signed_at: unsigned(row.get("signed_at_ns"))?,
        window_ends: unsigned(row.get("window_ends_ns"))?,
        revision: unsigned(row.get("revision"))?,
    })
}

/// A dispute's history line, as `Case::dispute` writes it.
const DISPUTED: &str = "escalated: disputed: ";

impl CaseStore {
    pub const fn new(db: Database) -> Self {
        Self { db }
    }

    pub fn ping(&self) -> Result<(), Error> {
        self.db.ping()
    }

    /// Every case not yet settled and the `settled` most recently settled, with holds and suspensions by hand.
    pub fn load(&self, settled: usize) -> Result<Held, Error> {
        let limit = i64::try_from(settled).unwrap_or(i64::MAX);
        let mut conn = self.db.get()?;
        let mut tx = conn.transaction()?;
        let rows = tx.query(
            "SELECT * FROM cases WHERE stage <> 'settled'
               OR event_id IN (SELECT event_id FROM cases WHERE stage = 'settled' ORDER BY updated_at DESC, event_id DESC
                               LIMIT $1)
             ORDER BY event_id",
            &[&limit],
        )?;
        let mut cases = Vec::new();
        for row in &rows {
            cases.push(Self::case_from(&mut tx, row)?);
        }
        let holds = tx
            .query("SELECT event_id, until_ns FROM holds", &[])?
            .iter()
            .map(|r| Ok((unsigned(r.get(0))?, unsigned(r.get(1))?)))
            .collect::<Result<_, Error>>()?;
        let suspended_by_hand = tx
            .query("SELECT event_id FROM suspended_by_hand", &[])?
            .iter()
            .map(|r| unsigned(r.get(0)))
            .collect::<Result<_, _>>()?;
        tx.commit()?;
        Ok(Held { cases, holds, suspended_by_hand })
    }

    fn case_from(tx: &mut Transaction<'_>, row: &Row) -> Result<Case, Error> {
        let event: i64 = row.get("event_id");
        let reports = tx
            .query("SELECT * FROM case_reports WHERE event_id = $1 ORDER BY n", &[&event])?
            .iter()
            .map(|r| {
                Ok(Report {
                    source: source(r.get("source"))?,
                    outcome: outcome_from(r.get::<_, &str>("outcome"), r.get("winning_state"))?,
                    evidence: r.get("evidence"),
                    at: unsigned(r.get("at_ns"))?,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let history = tx
            .query("SELECT at_ns, what FROM case_history WHERE event_id = $1 ORDER BY n", &[&event])?
            .iter()
            .map(|r| Ok((unsigned(r.get(0))?, r.get::<_, String>(1))))
            .collect::<Result<Vec<_>, Error>>()?;
        let since = || -> Result<u64, Error> {
            unsigned(row.get::<_, Option<i64>>("stage_since_ns").ok_or_else(|| Error::refused("no stage_since_ns"))?)
        };
        let proposal = |tx: &mut Transaction<'_>| -> Result<Proposal, Error> {
            let revision: i64 =
                row.get::<_, Option<i64>>("proposal_revision").ok_or_else(|| Error::refused("no proposal"))?;
            let p = tx
                .query_opt("SELECT * FROM case_proposals WHERE event_id = $1 AND revision = $2", &[&event, &revision])?
                .ok_or_else(|| Error::refused(format!("case {event}: no proposal {revision}")))?;
            let mut signatures = Vec::new();
            for s in tx.query(
                "SELECT signer FROM case_signoffs WHERE event_id = $1 AND revision = $2 ORDER BY signer",
                &[&event, &revision],
            )? {
                let signer: &str = s.get(0);
                let known = APPROVERS
                    .iter()
                    .find(|a| **a == signer)
                    .ok_or_else(|| Error::refused(format!("a signer {signer}")))?;
                signatures.push(*known);
            }
            proposal_from(&p, signatures)
        };
        let stage = match row.get::<_, &str>("stage") {
            "collecting" => Stage::Collecting,
            "tiebreak" => Stage::Tiebreak { since: since()? },
            "escalated" => Stage::Escalated {
                since: since()?,
                reason: row.get::<_, Option<String>>("escalation_reason").unwrap_or_default(),
            },
            "proposed" => Stage::Proposed(proposal(tx)?),
            "sequencing" => Stage::Sequencing(proposal(tx)?),
            "settled" => Stage::Settled(proposal(tx)?),
            other => return Err(Error::refused(format!("case {event}: a stage {other}"))),
        };
        Ok(Case::restore(
            unsigned(event)?,
            unsigned(row.get("states"))?,
            unsigned(row.get("opened_at_ns"))?,
            reports,
            stage,
            history,
            unsigned(row.get("revisions"))?,
        ))
    }

    /// Writes a case as it now stands: its row, and whatever reports, proposal, sign-offs, disputes and history it has
    /// that the records do not. Writing it again changes nothing.
    pub fn save(&self, case: &Case) -> Result<(), Error> {
        let event = big(case.event)?;
        let (stage, since, reason, proposal) = match &case.stage {
            Stage::Collecting => ("collecting", None, None, None),
            Stage::Tiebreak { since } => ("tiebreak", Some(*since), None, None),
            Stage::Escalated { since, reason } => ("escalated", Some(*since), Some(reason.as_str()), None),
            Stage::Proposed(p) => ("proposed", None, None, Some(p)),
            Stage::Sequencing(p) => ("sequencing", None, None, Some(p)),
            Stage::Settled(p) => ("settled", None, None, Some(p)),
        };
        let since = since.map(big).transpose()?;
        let revision = proposal.map(|p| big(p.revision)).transpose()?;
        self.db.transaction(|tx| {
            tx.execute(
                "INSERT INTO cases AS c (event_id, states, opened_at_ns, stage, stage_since_ns, escalation_reason,
                   proposal_revision, revisions, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
                 ON CONFLICT (event_id) DO UPDATE SET stage = EXCLUDED.stage, stage_since_ns = EXCLUDED.stage_since_ns,
                   escalation_reason = EXCLUDED.escalation_reason, proposal_revision = EXCLUDED.proposal_revision,
                   revisions = EXCLUDED.revisions, updated_at = now()
                 WHERE (c.stage, c.stage_since_ns, c.escalation_reason, c.proposal_revision, c.revisions)
                   IS DISTINCT FROM (EXCLUDED.stage, EXCLUDED.stage_since_ns, EXCLUDED.escalation_reason,
                                     EXCLUDED.proposal_revision, EXCLUDED.revisions)",
                &[&event, &big(case.states)?, &big(case.opened_at)?, &stage, &since, &reason, &revision, &big(case.revisions())?],
            )?;
            if let Some(p) = proposal {
                let (outcome, state) = outcome_columns(p.outcome);
                let sources: Vec<&str> = p.sources.iter().map(|s| s.name()).collect();
                tx.execute(
                    "INSERT INTO case_proposals (event_id, revision, outcome, winning_state, sources, evidence, signed_at_ns,
                       window_ends_ns)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING",
                    &[&event, &big(p.revision)?, &outcome, &state, &sources, &p.evidence, &big(p.signed_at)?, &big(p.window_ends)?],
                )?;
                for signer in &p.signatures {
                    tx.execute(
                        "INSERT INTO case_signoffs (event_id, revision, signer, signed_at_ns) VALUES ($1, $2, $3, $4)
                         ON CONFLICT DO NOTHING",
                        &[&event, &big(p.revision)?, signer, &big(p.signed_at)?],
                    )?;
                }
            }
            let written = |tx: &mut Transaction<'_>, table: &str| -> Result<usize, Error> {
                let n: i64 = tx.query_one(&format!("SELECT count(*) FROM {table} WHERE event_id = $1"), &[&event])?.get(0);
                Ok(usize::try_from(n).unwrap_or(0))
            };
            for (n, r) in case.reports.iter().enumerate().skip(written(tx, "case_reports")?) {
                let (outcome, state) = outcome_columns(r.outcome);
                tx.execute(
                    "INSERT INTO case_reports (event_id, n, source, outcome, winning_state, evidence, at_ns)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    &[&event, &number(n)?, &r.source.name(), &outcome, &state, &r.evidence, &big(r.at)?],
                )?;
            }
            for (n, (at, what)) in case.history.iter().enumerate().skip(written(tx, "case_history")?) {
                tx.execute(
                    "INSERT INTO case_history (event_id, n, at_ns, what) VALUES ($1, $2, $3, $4)",
                    &[&event, &number(n)?, &big(*at)?, what],
                )?;
                if let Some(reason) = what.strip_prefix(DISPUTED) {
                    tx.execute(
                        "INSERT INTO case_disputes (event_id, n, revision, reason, at_ns) VALUES ($1, $2, $3, $4, $5)",
                        &[&event, &number(n)?, &big(case.revisions())?, &reason, &big(*at)?],
                    )?;
                }
            }
            Ok(())
        })
    }

    /// Records a round postponed until `until`, or its postponement lifted.
    pub fn hold(&self, event: u64, until: Option<u64>) -> Result<(), Error> {
        let mut conn = self.db.get()?;
        match until {
            Some(until) => conn.execute(
                "INSERT INTO holds (event_id, until_ns) VALUES ($1, $2) ON CONFLICT (event_id) DO UPDATE SET until_ns = $2",
                &[&big(event)?, &big(until)?],
            )?,
            None => conn.execute("DELETE FROM holds WHERE event_id = $1", &[&big(event)?])?,
        };
        Ok(())
    }

    /// Records a round suspended by hand, or resumed.
    pub fn suspended_by_hand(&self, event: u64, suspended: bool) -> Result<(), Error> {
        let mut conn = self.db.get()?;
        if suspended {
            conn.execute(
                "INSERT INTO suspended_by_hand (event_id) VALUES ($1) ON CONFLICT DO NOTHING",
                &[&big(event)?],
            )?;
        } else {
            conn.execute("DELETE FROM suspended_by_hand WHERE event_id = $1", &[&big(event)?])?;
        }
        Ok(())
    }
}

fn number(n: usize) -> Result<i32, Error> {
    i32::try_from(n).map_err(|_| Error::refused("too many entries in a case"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::Timers;

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

    fn store(name: &str) -> Option<(Database, CaseStore)> {
        let config = venue_store::testing::owned_config(name)?;
        let db = Database::connect(&config).expect("the test database");
        db.migrate(MIGRATIONS).expect("migrated");
        Some((db.clone(), CaseStore::new(db)))
    }

    #[test]
    fn the_migration_files_are_the_ones_the_build_embeds() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        venue_store::check_files(&dir, MIGRATIONS).expect("the same migrations");
    }

    #[test]
    fn a_case_reads_back_as_it_was_written_at_every_stage() {
        let Some((db, store)) = store("oracle_cases") else { return };
        let (t, n) = (timers(), names());
        let mut case = Case::open(2_000_001, 3, 100 * S);
        let check = |case: &Case| {
            store.save(case).expect("written");
            store.save(case).expect("written again, changing nothing");
            let held = store.load(10).expect("read");
            assert_eq!(held.cases, vec![case.clone()]);
        };
        check(&case);
        case.report(report(Source::Alpha, Outcome::State(0), 100 * S), &t, &n);
        case.report(report(Source::Beta, Outcome::State(1), 101 * S), &t, &n);
        check(&case);
        assert!(matches!(case.stage, Stage::Tiebreak { .. }));
        case.report(report(Source::Referee, Outcome::State(1), 116 * S), &t, &n);
        check(&case);
        case.dispute("the match was replayed", 120 * S).expect("in the window");
        check(&case);
        case.operator_propose(Outcome::Void, "abandoned".into(), 130 * S, &t, &n).expect("open");
        check(&case);
        assert!(case.tick(160 * S, &t, &n).is_some());
        check(&case);
        case.settled(Outcome::Void, 161 * S, &n);
        check(&case);

        let mut conn = db.get().expect("a connection");
        let count = |conn: &mut venue_store::Conn, table: &str| -> i64 {
            conn.query_one(&format!("SELECT count(*) FROM {table}"), &[]).expect("a row").get(0)
        };
        assert_eq!(count(&mut conn, "case_reports"), 4, "alpha, beta, referee and the operator");
        assert_eq!(count(&mut conn, "case_proposals"), 2, "beta and the referee's, then the operator's void");
        assert_eq!(count(&mut conn, "case_signoffs"), 4, "two signatures each");
        assert_eq!(count(&mut conn, "case_disputes"), 1);
        let dispute: (i64, String) = conn
            .query_one("SELECT revision, reason FROM case_disputes", &[])
            .map(|r| (r.get(0), r.get(1)))
            .expect("a row");
        assert_eq!(dispute, (1, "the match was replayed".to_string()));
        for sql in [
            "UPDATE case_reports SET evidence = 'changed'",
            "DELETE FROM case_history",
            "UPDATE case_proposals SET outcome = 'state', winning_state = 1",
            "TRUNCATE case_signoffs",
            "DELETE FROM cases",
        ] {
            assert!(conn.batch_execute(sql).is_err(), "the database accepted: {sql}");
        }
    }

    #[test]
    fn manual_holds_and_suspensions_survive_and_only_recent_settled_cases_are_read() {
        let Some((_, store)) = store("oracle_manual") else { return };
        store.hold(7, Some(500 * S)).expect("held");
        store.hold(7, Some(600 * S)).expect("held longer");
        store.hold(8, Some(1)).expect("held");
        store.hold(8, None).expect("released");
        store.suspended_by_hand(9, true).expect("suspended");
        store.suspended_by_hand(10, true).expect("suspended");
        store.suspended_by_hand(10, false).expect("resumed");
        let n = names();
        for event in 1..=5 {
            let mut case = Case::open(event, 2, 0);
            if event < 5 {
                case.settled(Outcome::State(1), event * S, &n);
            }
            store.save(&case).expect("written");
        }
        let held = store.load(2).expect("read");
        assert_eq!(held.holds, BTreeMap::from([(7, 600 * S)]));
        assert_eq!(held.suspended_by_hand, BTreeSet::from([9]));
        let events: Vec<u64> = held.cases.iter().map(|c| c.event).collect();
        assert_eq!(events, vec![3, 4, 5], "the unsettled case and the two settled last");
    }
}
