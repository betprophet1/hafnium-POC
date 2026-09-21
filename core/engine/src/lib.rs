//! One sequencer group's engine: a deterministic state machine from journal inputs to results.
//!
//! The behaviour is normative in `spec/engine/engine-spec.md`, which this crate implements rule by rule;
//! rule numbers (`E-10.6`) in comments point at the sentence the code implements. The encoding is
//! `interfaces/sbe/venue-journal.xml`. The acceptance tests are the scenarios in `spec/engine/scenarios`
//! and the differential vectors from the reference model, both run by `./check`.
//!
//! Constraints that are not negotiable (blueprint §4, spec E-1.1):
//! - `no_std`, no allocation: every table lives in caller-provided [`state::Memory`];
//! - no clock, no I/O, no randomness of its own;
//! - no floating point: money is integer cents, prices are tick ordinals, ratios are exact rationals;
//! - no hash-ordered iteration: anything iterated is ordered by an explicit key;
//! - overflow is a fail-stop, never a wrap.

#![no_std]
#![forbid(unsafe_code)]

pub use venue_journal;

mod book;
mod check;
mod commands;
pub mod emit;
pub mod fees;
pub mod fixed;
pub mod frame;
mod orders;
mod settle;
pub mod snapshot;
pub mod state;
pub mod unit;

use emit::{Emitter, Out, Side};
use fixed::FixedMap;
use frame::Kind;
use state::*;
use unit::Touches;
use venue_journal::seed_purpose::SeedPurpose;
use venue_journal::self_match_prevention::SelfMatchPrevention;
use venue_journal::venue_mode::VenueMode;

/// Why the engine stopped (spec §18). Every replica stops at the same input for the same reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailStop {
    /// E-4.1: the input's sequence is not the next one, or its time is earlier than the engine's.
    SequenceOrTime,
    /// E-4.3: an input could not be decoded, or a command or sequencer event names another group.
    MalformedCommand,
    /// E-2.2: arithmetic on money or quantities overflowed.
    Overflow,
    /// E-3.3: an account's available cash fell below zero, or further below it, other than by a void or the fees
    /// the unit charged it.
    NegativeAvailable,
    /// E-10.10: a book was left crossed.
    CrossedBook,
    /// E-15.2, E-15.6: an event's payouts in some state, or at settlement, do not equal its collateral.
    Conservation,
    /// E-7.7: activation of a protocol version or binary this engine does not implement.
    UnsupportedProtocol,
    /// A table sized at construction is full, or its storage is unusable.
    Capacity,
    /// An internal invariant broke. The rules are constructed so a correct engine never stops for this.
    Inconsistent,
    /// The engine does not implement this input yet.
    NotImplemented,
    /// An earlier input already stopped the engine.
    Stopped,
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
    /// Seed for tables keyed by exchange-assigned IDs. Not observable in results.
    pub table_seed: u64,
}

/// The highest protocol version this build implements (E-7.7).
pub const PROTOCOL_VERSION: u16 = 1;

/// The engine's entire state (spec §3).
pub struct Engine<'a> {
    config: EngineConfig,
    stopped: bool,
    last_sequence: u64,
    now: u64,
    protocol_version: u16,
    venue_mode: VenueMode,
    active_order_limit: u32,
    client_order_id_window_seconds: u32,
    /// E-7.12: how long a resolved event is kept (D-29), and the listing limits, 0 for none (D-30).
    settled_retention_seconds: u32,
    max_events: u32,
    max_instruments: u32,
    max_price_levels: u32,
    /// The releases (E-8.4): the earliest and latest resolved events not yet released, linked by `next_release`.
    release_head: Option<u64>,
    release_tail: Option<u64>,
    default_limits: Option<RuleLimits>,
    next_order: u64,
    next_trade: u64,

    tick_tables: FixedMap<'a, u32, TickTable>,
    tick_rows: &'a mut [TickRow],
    tick_rows_used: usize,
    events: FixedMap<'a, u64, Event>,
    instruments: &'a mut [Instrument],
    /// Instrument slots ever used; those below it that a release freed are linked from `free_instrument`.
    instruments_used: usize,
    free_instrument: u32,
    /// Instruments the group carries (E-6.5).
    instruments_carried: usize,
    instrument_index: FixedMap<'a, u64, u32>,
    levels: &'a mut [Level],
    /// The end of the level pool's used part. Books take ranges from it; a release leaves a hole, and a listing that
    /// does not fit at the end compacts the pool (`commands.rs`), so what the group carries always fits (E-18.3).
    levels_used: usize,
    /// Levels of the books the group carries, holes not counted: its price levels (E-6.5).
    levels_carried: usize,
    level_bits: &'a mut [u64],
    level_bits_used: usize,
    level_bits_carried: usize,
    accounts: &'a mut [Account],
    accounts_used: usize,
    account_index: FixedMap<'a, u64, u32>,
    holdings: FixedMap<'a, HoldingKey, Holding>,
    orders: &'a mut [Order],
    free_order: u32,
    order_index: FixedMap<'a, u64, u32>,
    resting_orders: u32,
    duplicates: FixedMap<'a, (u64, [u8; 32]), Duplicate>,
    /// `(expiresAt, key)` for every recorded client order ID (E-8.2). Normally a queue in recording order, which is
    /// expiry order while the window never shrinks, so a tick removes from the front at no more than it costs to
    /// look. Entries keep the window in force when they were recorded (E-7.4), though, so once the window is
    /// shortened a later entry can expire before an earlier one, and a queue would strand it behind the earlier one
    /// until the table filled. From then until everything recorded under the longer window has gone, the records are
    /// a min-heap in `expiry[..expiry_len]` instead.
    expiry: &'a mut [(u64, (u64, [u8; 32]))],
    expiry_head: usize,
    expiry_len: usize,
    /// While the records are a heap: the latest expiry recorded before the window was last shortened.
    expiry_heap_until: Option<u64>,
    /// The latest expiry recorded so far.
    expiry_latest: u64,
    /// `(expiresAt, orderId)` for the resting orders that expire, as a min-heap, so E-8.1 costs what expires
    /// rather than what rests.
    gtd: &'a mut [(u64, u64)],
    gtd_len: usize,
    /// `commandId` to its expiry (E-7.1, D-25).
    commands: FixedMap<'a, [u8; 16], u64>,
    /// `(expiresAt, commandId)` for every recorded command ID, oldest first (E-8.2).
    command_expiry: &'a mut [(u64, [u8; 16])],
    command_expiry_head: usize,
    command_expiry_len: usize,
    order_groups: FixedMap<'a, (u64, u32), OrderGroup>,
    maker_settings: FixedMap<'a, HoldingKey, MakerSetting>,
    maker_counters: FixedMap<'a, HoldingKey, WindowCounter>,
    event_cash: FixedMap<'a, EventCashKey, i64>,
    /// The default rates (E-7.9, D-2).
    fee_schedule: FeeRates,
    /// Overrides, sorted by `(accountId, priority, overrideId)` in `fee_overrides[..fee_overrides_len]` (`fees.rs`).
    fee_overrides: &'a mut [FeeOverride],
    fee_overrides_len: usize,
    /// Numbers units, so fees charged in one are not counted against the next (E-3.3).
    unit_serial: u64,
    /// This input is a void, the one input that may take available cash below zero (E-3.3).
    voided: bool,

    position_touches: Touches<'a, HoldingKey, i64>,
    sleeve_touches: Touches<'a, u64, (i64, i64)>,
    level_touches: Touches<'a, LevelKey, (u64, u32)>,
    marks: &'a mut [Mark],
    marks_len: usize,
    scratch: &'a mut [(u64, u64)],
    dry: bool,
    undo: &'a mut [Undo],
    undo_len: usize,
    /// The fill check's views of orders and holdings (E-10.9), valid where stamped with `check_stamp`.
    check_orders: &'a mut [CheckOrder],
    check_holdings: &'a mut [CheckHolding],
    check_stamp: u32,
    emitter: Emitter,
}

#[inline]
fn cap<T>(r: Result<T, fixed::Full>) -> Result<T, FailStop> {
    r.map_err(|_| FailStop::Capacity)
}

impl<'a> Engine<'a> {
    /// Creates an engine in the initial state of E-3.1 over the given storage.
    pub fn new(config: EngineConfig, memory: Memory<'a>) -> Result<Self, FailStop> {
        let seed = config.table_seed;
        let orders = memory.orders;
        let order_count = orders.len();
        let scratch_needed =
            order_count.max(memory.instruments.len()).max(memory.holdings.len() / 2).max(memory.event_cash.len() / 2);
        let check_short =
            memory.check_orders.len() < order_count || memory.check_holdings.len() < memory.accounts.len();
        // Tables whose size follows another's, as `Memory` documents them. Short ones used to surface only as a
        // fail-stop mid-session; a host that sized them wrong now finds out before the first input.
        let derived_short = memory.gtd.len() < order_count.saturating_mul(2)
            || memory.expiry.len() < memory.duplicates.len()
            || memory.command_expiry.len() < memory.commands.len();
        if order_count >= NONE as usize || memory.scratch.len() < scratch_needed || check_short || derived_short {
            return Err(FailStop::Capacity);
        }
        let (check_orders, check_holdings) = (memory.check_orders, memory.check_holdings);
        check_orders.fill(CheckOrder::default());
        check_holdings.fill(CheckHolding::default());
        for (i, o) in orders.iter_mut().enumerate() {
            *o = Order {
                prev: NONE,
                next: NONE,
                account_next: if i + 1 < order_count { i as u32 + 1 } else { NONE },
                ..Order::default()
            };
        }
        let free_order = if orders.is_empty() { NONE } else { 0 };
        Ok(Self {
            config,
            stopped: false,
            last_sequence: 0,
            now: 0,
            protocol_version: 1,
            venue_mode: VenueMode::NORMAL,
            active_order_limit: 0,
            client_order_id_window_seconds: 7_200,
            settled_retention_seconds: 604_800,
            max_events: 0,
            max_instruments: 0,
            max_price_levels: 0,
            release_head: None,
            release_tail: None,
            default_limits: None,
            next_order: 1,
            next_trade: 1,
            tick_tables: cap(FixedMap::new(memory.tick_tables, seed))?,
            tick_rows: memory.tick_rows,
            tick_rows_used: 0,
            events: cap(FixedMap::new(memory.events, seed))?,
            instruments: memory.instruments,
            instruments_used: 0,
            free_instrument: NONE,
            instruments_carried: 0,
            instrument_index: cap(FixedMap::new(memory.instrument_index, seed))?,
            levels: memory.levels,
            levels_used: 0,
            levels_carried: 0,
            level_bits: memory.level_bits,
            level_bits_used: 0,
            level_bits_carried: 0,
            accounts: memory.accounts,
            accounts_used: 0,
            account_index: cap(FixedMap::new(memory.account_index, seed))?,
            holdings: cap(FixedMap::new(memory.holdings, seed))?,
            orders,
            free_order,
            order_index: cap(FixedMap::new(memory.order_index, seed))?,
            resting_orders: 0,
            duplicates: cap(FixedMap::new(memory.duplicates, seed))?,
            expiry: memory.expiry,
            expiry_head: 0,
            expiry_len: 0,
            expiry_heap_until: None,
            expiry_latest: 0,
            gtd: memory.gtd,
            gtd_len: 0,
            commands: cap(FixedMap::new(memory.commands, seed))?,
            command_expiry: memory.command_expiry,
            command_expiry_head: 0,
            command_expiry_len: 0,
            order_groups: cap(FixedMap::new(memory.order_groups, seed))?,
            maker_settings: cap(FixedMap::new(memory.maker_settings, seed))?,
            maker_counters: cap(FixedMap::new(memory.maker_counters, seed))?,
            event_cash: cap(FixedMap::new(memory.event_cash, seed))?,
            fee_schedule: FeeRates::default(),
            fee_overrides: memory.fee_overrides,
            fee_overrides_len: 0,
            unit_serial: 0,
            voided: false,
            position_touches: Touches::new(memory.position_touches),
            sleeve_touches: Touches::new(memory.sleeve_touches),
            level_touches: Touches::new(memory.level_touches),
            marks: memory.marks,
            marks_len: 0,
            scratch: memory.scratch,
            dry: false,
            undo: memory.undo,
            undo_len: 0,
            check_orders,
            check_holdings,
            check_stamp: 0,
            emitter: Emitter::new(config.sequencer_group),
        })
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
    /// Returns `Err` exactly when the specification requires a fail-stop; the engine then refuses every
    /// later input.
    pub fn apply<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        if self.stopped {
            return Err(FailStop::Stopped);
        }
        let outcome = self.apply_inner(input, sink);
        if outcome.is_err() {
            self.stopped = true;
        }
        outcome
    }

    fn apply_inner<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        let frame = frame::check(input).map_err(|_| FailStop::MalformedCommand)?;
        let record = RecordHeader::read(input);
        // E-4.1
        if record.sequence != self.last_sequence.wrapping_add(1) || record.sequenced_at < self.now {
            return Err(FailStop::SequenceOrTime);
        }
        self.last_sequence = record.sequence;
        self.now = record.sequenced_at;
        self.emitter.begin(record.sequence, record.sequenced_at);
        self.marks_len = 0;
        self.voided = false;
        // E-4.3: another group's input
        let foreign = record.sequencer_group != self.config.sequencer_group;
        if foreign && frame.kind != Kind::Client {
            return Err(FailStop::MalformedCommand);
        }
        self.dispatch(frame.template_id, foreign, input, sink)
    }

    fn dispatch<S: ResultSink>(
        &mut self,
        template: u16,
        foreign: bool,
        input: &[u8],
        sink: &mut S,
    ) -> Result<(), FailStop> {
        use venue_journal::*;
        match template {
            time_tick_codec::SBE_TEMPLATE_ID => self.on_time_tick(sink),
            engine_started_codec::SBE_TEMPLATE_ID => Ok(()),
            seed_set_codec::SBE_TEMPLATE_ID => self.on_seed_set(input),
            t => match frame::kind_of(t) {
                Some(Kind::Command) => self.on_command(t, input, sink),
                Some(Kind::Client) => self.on_client(t, foreign, input, sink),
                _ => Err(FailStop::MalformedCommand),
            },
        }
    }

    // ------------------------------------------------------------------ sequencer events (§8)

    fn on_time_tick<S: ResultSink>(&mut self, sink: &mut S) -> Result<(), FailStop> {
        // E-8.1: the heap hands back what expires in `(expiresAt, orderId)` order, which is the order they must go
        // in, and the work is what actually expired instead of every resting order.
        let now = self.now;
        while let Some((at, id)) = self.gtd_pop_due(now) {
            // A record outlives an order cancelled or filled before its time, so only an order that still rests and
            // still expires then goes here.
            let Some(&slot) = self.order_index.get(&id) else { continue };
            let o = self.orders[slot as usize];
            if o.in_use && o.expires_at == Some(at) {
                self.remove(slot, emit::CancelReason::EXPIRED, sink)?;
            }
        }
        // E-8.4: then at most one resolved event whose retention has passed.
        self.release_due(sink)?;
        // E-8.2: expired records are always at the front, of the queue or of the heap, so the work is what actually
        // expired, never the size of the table. The ID may have been recorded again under a later expiry; that
        // recording is its own record, so only an entry that has itself expired goes.
        let now = self.now;
        if let Some(until) = self.expiry_heap_until {
            while let Some((_, key)) = book::heap_pop_due(self.expiry, &mut self.expiry_len, now) {
                if self.duplicates.get(&key).is_some_and(|d| d.expires_at <= now) {
                    self.duplicates.remove(&key);
                }
            }
            if now >= until {
                // Everything recorded under a longer window has gone. What is left was recorded under windows no
                // longer than the current one, so sorted it is in recording order again, and a queue again.
                self.expiry[..self.expiry_len].sort_unstable();
                self.expiry_head = 0;
                self.expiry_heap_until = None;
            }
        } else {
            while self.expiry_len > 0 {
                let (expires_at, key) = self.expiry[self.expiry_head];
                if now < expires_at {
                    break;
                }
                if self.duplicates.get(&key).is_some_and(|d| d.expires_at <= now) {
                    self.duplicates.remove(&key);
                }
                self.expiry_head = (self.expiry_head + 1) % self.expiry.len();
                self.expiry_len -= 1;
            }
        }
        // E-8.2 for command IDs, the same way.
        while self.command_expiry_len > 0 {
            let (expires_at, id) = self.command_expiry[self.command_expiry_head];
            if now < expires_at {
                break;
            }
            if self.commands.get(&id).is_some_and(|&e| e <= now) {
                self.commands.remove(&id);
            }
            self.command_expiry_head = (self.command_expiry_head + 1) % self.command_expiry.len();
            self.command_expiry_len -= 1;
        }
        self.end_unit(sink)
    }

    fn on_seed_set(&mut self, input: &[u8]) -> Result<(), FailStop> {
        use venue_journal::seed_set_codec::SeedSetDecoder;
        let dec = SeedSetDecoder::default().header(commands::header(input), 0);
        match dec.purpose() {
            // E-8.3. Hashing is not observable; a table already holding entries keeps its seed.
            SeedPurpose::CLIENT_ORDER_ID_HASH | SeedPurpose::PRNG => Ok(()),
            SeedPurpose::NullVal => Err(FailStop::MalformedCommand),
        }
    }

    // ------------------------------------------------------------------ order removal by commands (§15, §16)

    fn cancel_instrument_orders<S: ResultSink>(
        &mut self,
        instrument_slot: u32,
        reason: emit::CancelReason,
        sink: &mut S,
    ) -> Result<(), FailStop> {
        self.remove_where(|o, _| o.instrument_slot == instrument_slot, reason, sink)
    }

    fn cancel_at_event_start<S: ResultSink>(&mut self, event_id: u64, sink: &mut S) -> Result<(), FailStop> {
        let keep =
            |o: &Order, inst: &Instrument| inst.event_id == event_id && o.at_event_start == emit::AtEventStart::CANCEL;
        self.remove_where(keep, emit::CancelReason::EVENT_START, sink)
    }

    fn cancel_all_orders<S: ResultSink>(&mut self, reason: emit::CancelReason, sink: &mut S) -> Result<(), FailStop> {
        self.remove_where(|_, _| true, reason, sink)
    }

    // ------------------------------------------------------------------ accounts

    /// The slot of an account, creating it with the initial values of E-3.1 on first mention (E-3.2).
    fn account_slot(&mut self, id: u64) -> Result<u32, FailStop> {
        if let Some(&slot) = self.account_index.get(&id) {
            return Ok(slot);
        }
        let slot = self.accounts_used;
        if slot >= self.accounts.len() {
            return Err(FailStop::Capacity);
        }
        self.accounts[slot] = Account {
            id,
            self_match_prevention: SelfMatchPrevention::CANCEL_INCOMING,
            first_order: NONE,
            ..Account::default()
        };
        cap(self.account_index.insert(id, slot as u32))?;
        self.accounts_used += 1;
        Ok(slot as u32)
    }

    /// Records a command ID (E-7.1) and queues its expiry for E-8.2.
    pub(crate) fn record_command(&mut self, id: [u8; 16], expires_at: u64) -> Result<(), FailStop> {
        if self.command_expiry_len >= self.command_expiry.len() {
            return Err(FailStop::Capacity);
        }
        cap(self.commands.insert(id, expires_at))?;
        let at = (self.command_expiry_head + self.command_expiry_len) % self.command_expiry.len();
        self.command_expiry[at] = (expires_at, id);
        self.command_expiry_len += 1;
        Ok(())
    }

    /// Records a client order ID (E-9.2) and queues its expiry for E-8.2.
    pub(crate) fn record_duplicate(&mut self, key: (u64, [u8; 32]), entry: Duplicate) -> Result<(), FailStop> {
        if self.expiry_len >= self.expiry.len() {
            return Err(FailStop::Capacity);
        }
        cap(self.duplicates.insert(key, entry))?;
        let record = (entry.expires_at, key);
        if self.expiry_heap_until.is_some() {
            // Cannot be full: checked above, before the map was touched.
            book::heap_push(self.expiry, &mut self.expiry_len, record);
        } else {
            let at = (self.expiry_head + self.expiry_len) % self.expiry.len();
            self.expiry[at] = record;
            self.expiry_len += 1;
        }
        self.expiry_latest = self.expiry_latest.max(entry.expires_at);
        Ok(())
    }

    /// The client order ID window is about to shrink (E-7.4): records made from now on can expire before ones
    /// already made, so they are kept as a heap until the last of those has expired (see `expiry`).
    pub(crate) fn expiry_window_shortened(&mut self) {
        if self.expiry_heap_until.is_none() {
            // The queue is in expiry order, and a sorted array is already a min-heap once it starts at the front.
            self.expiry.rotate_left(self.expiry_head);
            self.expiry_head = 0;
        }
        self.expiry_heap_until = Some(self.expiry_heap_until.map_or(self.expiry_latest, |u| u.max(self.expiry_latest)));
    }

    fn holding(&self, account_id: u64, instrument_id: u64) -> Holding {
        self.holdings.get(&(account_id, instrument_id)).copied().unwrap_or_default()
    }

    // ------------------------------------------------------------------ units (§5)

    /// Records a position's value before it changes (E-5.2 block 3). Not during a dry run.
    fn touch_position(&mut self, account_id: u64, instrument_id: u64) -> Result<(), FailStop> {
        if self.dry {
            return Ok(());
        }
        let value = self.holding(account_id, instrument_id).position;
        self.position_touches.touch((account_id, instrument_id), value).map_err(|_| FailStop::Capacity)
    }

    /// Records a sleeve's values before they change (E-5.2 block 4). Not during a dry run.
    fn touch_sleeve(&mut self, account_slot: u32) -> Result<(), FailStop> {
        if self.dry {
            return Ok(());
        }
        let a = &self.accounts[account_slot as usize];
        let value = (a.balance, a.reserved);
        self.sleeve_touches.touch(a.id, value).map_err(|_| FailStop::Capacity)
    }

    /// Records a book level's aggregates before they change (E-5.2 block 5). Not during a dry run.
    fn touch_level(&mut self, instrument_slot: u32, side: Side, tick: u16) -> Result<(), FailStop> {
        if self.dry {
            return Ok(());
        }
        let inst = &self.instruments[instrument_slot as usize];
        let level = self.levels[level_index(inst, side, tick)];
        let key = (inst.id, side_rank(side), tick);
        self.level_touches.touch(key, (level.quantity, level.count)).map_err(|_| FailStop::Capacity)
    }

    /// Checks E-10.10 on every book the unit touched, then emits blocks 3 to 5 of E-5.2.
    fn end_unit<S: ResultSink>(&mut self, sink: &mut S) -> Result<(), FailStop> {
        for &((instrument_id, _, _), _, _) in self.level_touches.entries() {
            let slot = *self.instrument_index.get(&instrument_id).ok_or(FailStop::Inconsistent)?;
            if let (Some(bid), Some(ask)) = (self.best(slot, Side::BUY), self.best(slot, Side::SELL)) {
                if bid >= ask {
                    return Err(FailStop::CrossedBook);
                }
            }
        }
        let Self {
            position_touches,
            sleeve_touches,
            level_touches,
            holdings,
            accounts,
            account_index,
            instruments,
            instrument_index,
            levels,
            emitter,
            voided,
            unit_serial,
            ..
        } = self;
        let unit = *unit_serial;
        *unit_serial = unit.checked_add(1).ok_or(FailStop::Overflow)?;
        position_touches.finish(
            |k| holdings.get(k).map_or(0, |h| h.position),
            |k, _, position| {
                emitter.emit(sink, &Out::PositionChanged { account_id: k.0, instrument_id: k.1, position })
            },
        )?;
        sleeve_touches.finish(
            |id| {
                account_index.get(id).map_or((0, 0), |&s| (accounts[s as usize].balance, accounts[s as usize].reserved))
            },
            |id, (start_balance, start_reserved), (balance, reserved)| {
                // E-3.3: only a void, or the fees the unit charged, take available cash below zero or further
                // below it. Account 0 pays rebates, so it is exempt.
                let available = balance.checked_sub(reserved).ok_or(FailStop::Overflow)?;
                let before = start_balance.checked_sub(start_reserved).ok_or(FailStop::Overflow)?;
                let fees = account_index.get(id).map_or(0, |&s| {
                    let a = &accounts[s as usize];
                    if a.fees_unit == unit {
                        a.fees_charged
                    } else {
                        0
                    }
                });
                let floor = before.min(0).checked_sub(fees).ok_or(FailStop::Overflow)?;
                if available < floor && !*voided && *id != fees::FEE_ACCOUNT {
                    return Err(FailStop::NegativeAvailable);
                }
                emitter.emit(
                    sink,
                    &Out::SleeveChanged { account_id: *id, available_cents: available, reserved_cents: reserved },
                )
            },
        )?;
        level_touches.finish(
            |k| {
                instrument_index.get(&k.0).map_or((0, 0), |&slot| {
                    let inst = &instruments[slot as usize];
                    let l = levels[level_index(inst, side_of_rank(k.1), k.2)];
                    (l.quantity, l.count)
                })
            },
            |k, _, (quantity, count)| {
                let quantity = u32::try_from(quantity).map_err(|_| FailStop::Overflow)?;
                emitter.emit(
                    sink,
                    &Out::BookLevelChanged {
                        instrument_id: k.0,
                        side: side_of_rank(k.1),
                        tick: k.2,
                        quantity,
                        order_count: count,
                    },
                )
            },
        )
    }
}

/// The record header every journal input starts with, read at its fixed offset (E-4.1).
struct RecordHeader {
    sequence: u64,
    sequenced_at: u64,
    sequencer_group: u16,
}

impl RecordHeader {
    fn read(input: &[u8]) -> Self {
        // frame::check guarantees the fixed block, which starts with this header, is present.
        let at = venue_journal::message_header_codec::ENCODED_LENGTH;
        let u64_at = |i: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&input[i..i + 8]);
            u64::from_le_bytes(b)
        };
        Self {
            sequence: u64_at(at),
            sequenced_at: u64_at(at + 8),
            sequencer_group: u16::from_le_bytes([input[at + 16], input[at + 17]]),
        }
    }
}

/// BUY sorts before SELL (E-5.2 block 5).
#[inline]
const fn side_rank(side: Side) -> u8 {
    match side {
        Side::BUY => 0,
        _ => 1,
    }
}

#[inline]
const fn side_of_rank(rank: u8) -> Side {
    if rank == 0 {
        Side::BUY
    } else {
        Side::SELL
    }
}

/// The level pool index of one tick on one side of a book.
#[inline]
fn level_index(inst: &Instrument, side: Side, tick: u16) -> usize {
    let per_side = usize::from(inst.table.len);
    inst.levels_start as usize + usize::from(side_rank(side)) * per_side + usize::from(tick)
}
