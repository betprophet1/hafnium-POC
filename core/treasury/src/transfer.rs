//! A deposit or withdrawal as interface 1 describes it (`Transfer` in interfaces/gateway/openapi.yaml), and the
//! finer stage the treasury tracks behind its state.

use crate::clock;
use serde_json::{json, Value};

/// Names as the contract spells them, both ways.
macro_rules! named {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const fn name(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn parse(text: &str) -> Option<Self> {
                match text { $($text => Some(Self::$variant),)+ _ => None }
            }

            pub const NAMES: &'static [&'static str] = &[$($text),+];
        }
    };
}

named!(Kind { Deposit => "deposit", Withdrawal => "withdrawal" });
named!(Method { DummyBank => "dummy_bank", DummyInstant => "dummy_instant", DummyFailing => "dummy_failing" });
named!(State { Pending => "pending", Settled => "settled", Failed => "failed", Cancelled => "cancelled" });
named!(Reason {
    DeclinedByBank => "declined_by_bank",
    ReturnedByBank => "returned_by_bank",
    InsufficientAvailableCash => "insufficient_available_cash",
    CancelledByClient => "cancelled_by_client",
});

/// Where a transfer is, finer than the state a client sees. Every stage but `Done` has something left to do, which
/// `Treasury::advance` does when it can.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// A withdrawal whose reclaim the sequencer group has not decided yet.
    Reclaiming,
    /// Accepted and not yet handed to the bank.
    ToBank,
    /// With the bank, which will settle it or fail it once `due` (wall-clock milliseconds) has passed.
    AtBank {
        due: u64,
        settles: bool,
    },
    /// A grant to send: a deposit the bank settled, or a withdrawal coming back. The transfer becomes `then` once
    /// the sequencer group has applied it.
    Granting {
        then: State,
        reason: Option<Reason>,
    },
    Done,
}

#[derive(Clone, Debug)]
pub struct Transfer {
    pub id: u64,
    pub account: u64,
    pub client_id: String,
    pub kind: Kind,
    pub amount: u64,
    pub method: Method,
    pub state: State,
    pub reason: Option<Reason>,
    pub sequence: Option<u64>,
    pub created: u64,
    pub updated: u64,
    pub stage: Stage,
}

impl Transfer {
    /// What a client is shown: interface 1's `Transfer`.
    pub fn to_json(&self) -> Value {
        json!({
            "transferId": self.id.to_string(), "clientTransferId": self.client_id, "kind": self.kind.name(),
            "amountCents": self.amount, "method": self.method.name(), "state": self.state.name(),
            "reason": self.reason.map(Reason::name), "sequence": self.sequence.map(|s| s.to_string()),
            "createdAt": clock::rfc3339(self.created), "updatedAt": clock::rfc3339(self.updated)
        })
    }

    /// What a client can see change: `updatedAt` moves only with these.
    pub const fn visible(&self) -> (State, Option<Reason>, Option<u64>) {
        (self.state, self.reason, self.sequence)
    }
}
