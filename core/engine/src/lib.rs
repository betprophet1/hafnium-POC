//! One sequencer group's engine: a deterministic state machine from journal inputs to results.
//!
//! The behaviour is normative in `spec/engine/engine-spec.md`, which this crate implements rule by rule;
//! cite rule numbers (`E-10.6`) in comments where code implements them. The encoding is
//! `interfaces/sbe/venue-journal.xml`. The acceptance tests are the scenarios in `spec/engine/scenarios`
//! and the differential vectors from the reference model, both run by `core/ci.sh`.
//!
//! Work is split into packages in `core/WORK_PACKAGES.md`. Until they land, [`Engine::apply`] reports
//! [`FailStop::NotImplemented`] and every scenario fails.
//!
//! Constraints that are not negotiable (blueprint §4, spec E-1.1):
//! - `no_std`, no allocation after construction, no clock, no I/O, no randomness of its own;
//! - no floating point: money is integer cents, prices are tick ordinals, ratios are exact rationals;
//! - no hash-ordered iteration: anything iterated is ordered by an explicit key;
//! - overflow is a fail-stop, never a wrap.

#![no_std]
#![forbid(unsafe_code)]

pub use venue_journal;

/// Why the engine stopped (spec §18). Every replica stops at the same input for the same reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailStop {
    /// E-4.1: the input's sequence is not the next one, or its time is earlier than the engine's.
    SequenceOrTime,
    /// E-4.3: a command or sequencer event could not be decoded, or names another group.
    MalformedCommand,
    /// E-2.2: arithmetic on money or quantities overflowed.
    Overflow,
    /// E-3.3: an account's available cash went negative.
    NegativeAvailable,
    /// E-10.10: a book was left crossed.
    CrossedBook,
    /// E-15.2: an event's payouts in some state no longer equal its collateral.
    Conservation,
    /// E-7.7: activation of a protocol version or binary this engine does not implement.
    UnsupportedProtocol,
    /// A result did not fit the sink's buffer, or an internal capacity was exceeded.
    Capacity,
    /// The engine does not implement this yet.
    NotImplemented,
}

/// Receives each encoded result, in order (spec §5). The engine never allocates to emit a result.
pub trait ResultSink {
    fn emit(&mut self, encoded: &[u8]);
}

/// Fixed at construction; nothing here may change while the engine runs.
#[derive(Clone, Copy, Debug)]
pub struct EngineConfig {
    /// The sequencer group this engine serves (E-2.8, E-4.3).
    pub sequencer_group: u16,
    /// SHA-256 of this engine binary, checked by `ActivateProtocol` (E-7.7).
    pub binary_hash: [u8; 32],
}

/// The engine's entire state (spec §3). Every table is sized at construction (work package WP-2).
pub struct Engine {
    config: EngineConfig,
    last_sequence: u64,
    now: u64,
}

impl Engine {
    /// Creates an engine in the initial state of E-3.1.
    pub const fn new(config: EngineConfig) -> Self {
        Self {
            config,
            last_sequence: 0,
            now: 0,
        }
    }

    /// The configuration this engine was created with.
    pub const fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// The last input sequence applied and the engine's current time (E-4.1).
    pub const fn position(&self) -> (u64, u64) {
        (self.last_sequence, self.now)
    }

    /// Applies one journal input, given as SBE bytes, and emits its results to `sink` in order.
    ///
    /// Returns `Err` exactly when the specification requires a fail-stop; after one, the engine must not
    /// be used again.
    pub fn apply<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        let _ = (input, sink);
        Err(FailStop::NotImplemented)
    }
}
