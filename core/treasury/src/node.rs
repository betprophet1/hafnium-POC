//! The sequencer group behind a node's ingress, and what its feed says about the treasury's own commands.
//!
//! A command sent again after a restart is answered `DUPLICATE` (spec E-7.1), which says it was decided before but not
//! how. The feed says how: it replays the whole journal to each new follower, so the first outcome of every command
//! this book has sent is there to be found. This is the outbox relay of blueprint §18 in miniature: a command is done
//! only once its outcome has been seen.

use crate::treasury::{Adjusted, Journal, Recorded, Sequencer};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use venue_link::wire::{self, Ingress};

/// Every one of the treasury's commands seen on the feed, with its first outcome, and how far the feed has been read.
/// Reconciliation reads it whole; a duplicate answer reads one command's outcome from it.
type Seen = Arc<Mutex<Journal>>;

pub struct Node {
    ingress: Ingress,
    seen: Seen,
}

fn adjusted(sequence: u64, outcome: &str, reason: &Value) -> Option<Adjusted> {
    match outcome {
        "APPLIED" => Some(Adjusted::Applied(sequence)),
        "REJECTED" => Some(Adjusted::Rejected(reason.as_str().unwrap_or("").to_lowercase())),
        _ => None,
    }
}

impl Node {
    /// Sends to the ingress at `ingress`, and follows the feed at `feed` for the outcomes of commands whose IDs start
    /// with `prefix`.
    pub fn new(ingress: &str, feed: &str, prefix: String) -> Self {
        let seen: Seen = Arc::default();
        let (journal, feed) = (Arc::clone(&seen), feed.to_string());
        thread::spawn(move || {
            wire::follow(
                &feed,
                || 0,
                || {},
                |unit| {
                    let sequence = unit.input["record"]["sequence"].as_u64().unwrap_or(0);
                    let mut journal = journal.lock().unwrap_or_else(PoisonError::into_inner);
                    journal.through = journal.through.max(sequence);
                    let id = unit.input["commandId"].as_str().unwrap_or("");
                    if unit.input["msg"] != "SleeveAdjust" || !id.starts_with(&prefix) {
                        return;
                    }
                    let first =
                        unit.results.iter().find(|r| r["msg"] == "CommandApplied" && r["outcome"] != "DUPLICATE");
                    let Some(r) = first else { return };
                    let applied_at = r["result"]["inputSequence"].as_u64().unwrap_or(sequence);
                    let Some(outcome) = adjusted(applied_at, r["outcome"].as_str().unwrap_or(""), &r["rejectReason"])
                    else {
                        return;
                    };
                    journal.commands.entry(id.to_string()).or_insert(Recorded {
                        account: unit.input["accountId"].as_u64().unwrap_or(0),
                        cents: unit.input["deltaCents"].as_u64().unwrap_or(0),
                        reason: unit.input["reason"].as_str().unwrap_or("").to_string(),
                        outcome,
                    });
                },
            );
        });
        Self { ingress: Ingress::new(ingress), seen }
    }
}

impl Sequencer for Node {
    fn adjust(&mut self, command_id: &str, account: u64, cents: u64, reason: &str) -> Result<Adjusted, String> {
        let input = wire::command(
            "SleeveAdjust",
            command_id.to_string(),
            json!({"accountId": account, "deltaCents": cents, "reason": reason}),
        );
        let results = self.ingress.send(input).map_err(|e| e.to_string())?;
        let applied = results
            .iter()
            .find(|r| r["msg"] == "CommandApplied")
            .ok_or("the sequencer group answered without an outcome")?;
        let sequence = applied["result"]["inputSequence"].as_u64().unwrap_or(0);
        let outcome = applied["outcome"].as_str().unwrap_or("");
        if let Some(adjusted) = adjusted(sequence, outcome, &applied["rejectReason"]) {
            return Ok(adjusted);
        }
        // A duplicate: its first outcome is on the feed, or will be once the feed catches up.
        let first = self.seen.lock().unwrap_or_else(PoisonError::into_inner).commands.get(command_id).cloned();
        Ok(first.map_or(Adjusted::Unknown, |r| r.outcome))
    }

    fn journal(&self) -> Journal {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}
