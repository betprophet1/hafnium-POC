//! The records of spec §3 and the caller-provided storage they live in.
//!
//! The engine never allocates. A host sizes every table with [`Capacities`], allocates the slices of a
//! [`Memory`] once, and hands them to [`crate::Engine::new`]. Slot indices never leave the engine (blueprint §6).

use crate::emit::{AtEventStart, ClientOrderId, CommandId, MarketState, Side};
use venue_journal::allocation_policy::AllocationPolicy;
use venue_journal::instrument_class::InstrumentClass;
use venue_journal::self_match_prevention::SelfMatchPrevention;
use venue_journal::time_in_force::TimeInForce;

/// Marks the end of an intrusive list.
pub const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickRow {
    pub numerator: u32,
    pub denominator: u32,
    pub complement: u16,
}

/// A tick table: rows `start..start + len` of the row pool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickTable {
    pub start: u32,
    pub len: u16,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Event {
    pub state_count: u8,
    pub scheduled_start: u64,
    pub started: bool,
    pub suspended: bool,
    pub ended: bool,
    pub resolved: bool,
    /// When the event was resolved (E-15.6), from which its release is counted (E-8.4).
    pub resolved_at: u64,
    /// The next event in the releases, in resolution order (E-8.4).
    pub next_release: Option<u64>,
    pub collateral: i64,
    /// Instrument slots of this event, ascending by instrument ID.
    pub first_instrument: u32,
    pub instrument_count: u16,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Instrument {
    /// False for a slot never used or freed by a release (E-8.4); a free slot's `next_in_event` links the free list.
    pub in_use: bool,
    pub id: u64,
    pub event_id: u64,
    pub class: InstrumentClass,
    pub table: TickTable,
    pub lot_step: u32,
    pub policy: AllocationPolicy,
    pub mask: u64,
    pub position_limit: u32,
    pub market_state: MarketState,
    pub next_in_event: u32,
    /// Level pool index of this book's BUY levels; SELL levels follow them.
    pub levels_start: u32,
    /// Bitmap pool index of this book's BUY occupancy words; SELL words follow them.
    pub bits_start: u32,
}

impl Instrument {
    pub fn words_per_side(&self) -> usize {
        usize::from(self.table.len).div_ceil(64)
    }
}

/// One price level of one side of a book: a FIFO of order slots and its aggregates.
#[derive(Clone, Copy, Debug, Default)]
pub struct Level {
    pub quantity: u64,
    pub count: u32,
    pub head: u32,
    pub tail: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Order {
    pub in_use: bool,
    pub id: u64,
    pub account_id: u64,
    pub account_slot: u32,
    pub session_id: u64,
    pub client_order_id: ClientOrderId,
    pub instrument_slot: u32,
    pub side: Side,
    pub tick: u16,
    pub remaining: u32,
    pub time_in_force: TimeInForce,
    pub expires_at: Option<u64>,
    pub at_event_start: AtEventStart,
    pub post_only: bool,
    pub reduce_only: bool,
    pub spend_cap_remaining: Option<i64>,
    pub group: Option<u32>,
    pub accepted_at: u64,
    pub lock: u32,
    pub reserved: i64,
    /// Neighbours in the level FIFO.
    pub prev: u32,
    pub next: u32,
    /// Neighbours in the account's list of resting orders; also the free list link when not in use.
    pub account_prev: u32,
    pub account_next: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuleLimits {
    pub max_resting_orders: u32,
    pub max_new_orders_per_second: u32,
    pub max_order_to_trade_ratio_milli: u32,
    pub ratio_free_orders_per_second: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Account {
    pub id: u64,
    pub balance: i64,
    pub reserved: i64,
    pub closed_only: bool,
    pub self_match_prevention: SelfMatchPrevention,
    /// 0 = none, else the account whose family this one shares for self-match prevention (E-3.2, D-33). Set once
    /// by `SetMasterAccount` (E-7.13) and never reassigned.
    pub master_account_id: u64,
    pub second: Option<u64>,
    pub new_orders_in_second: u32,
    pub fills_in_second: u32,
    pub limits: Option<RuleLimits>,
    pub first_order: u32,
    pub resting_orders: u32,
    /// Fees charged to the account in the unit numbered `fees_unit`, rebates not counted: how far below its bound
    /// E-3.3 lets that unit take the account's available cash (D-26).
    pub fees_charged: i64,
    pub fees_unit: u64,
}

/// An account's position in one instrument and the holdings its resting orders lock (E-11.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Holding {
    pub position: i64,
    pub yes_locked: i64,
    pub no_locked: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OrderGroup {
    pub threshold: u32,
    pub window_millis: u32,
    pub count: u64,
    pub window_start: Option<u64>,
    pub triggered: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MakerSetting {
    pub threshold: u32,
    pub window_millis: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowCounter {
    pub count: u64,
    pub window_start: Option<u64>,
}

/// A maker and a taker rate, in millionths (E-10.11).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeeRates {
    pub maker_micros: i32,
    pub taker_micros: i32,
}

/// An override of the fee schedule (E-7.10). An `account_id`, `instrument_id` or `event_id` of 0 matches any.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeeOverride {
    pub account_id: u64,
    pub priority: u32,
    pub id: u64,
    pub instrument_id: u64,
    pub event_id: u64,
    pub maker_rate_micros: i32,
    pub taker_rate_micros: i32,
    pub expires_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Duplicate {
    /// When the entry stops making orders duplicates: recorded under the window in force then (E-9.2).
    pub expires_at: u64,
    pub order_id: Option<u64>,
}

/// An order as the fill check (E-10.9) sees it partway through its walk: what remains of it, what it locks, and
/// whether it still rests. Stands only while `stamp` is the engine's current check; otherwise the order's own
/// record does.
#[derive(Clone, Copy, Debug, Default)]
pub struct CheckOrder {
    pub stamp: u32,
    pub remaining: u32,
    pub lock: u32,
    pub resting: bool,
}

/// An account's holding in the instrument being checked, as the fill check sees it partway through its walk.
/// Stands only while `stamp` is the engine's current check. One instrument per walk, so one per account.
#[derive(Clone, Copy, Debug, Default)]
pub struct CheckHolding {
    pub stamp: u32,
    pub holding: Holding,
}

/// A value a dry run (E-10.9) overwrote, restored when the dry run ends.
#[derive(Clone, Copy, Debug)]
pub enum Undo {
    Order(u32, Order),
    Level(u32, Level),
    Bits(u32, u64),
    Account(u32, Account),
    Holding(HoldingKey, Option<Holding>),
    OrderIndex(u64, Option<u32>),
    Collateral(u64, i64),
    EventCash(EventCashKey, Option<i64>),
    MakerCounter(HoldingKey, Option<WindowCounter>),
    Group((u64, u32), Option<OrderGroup>),
    Scalars { next_trade: u64, free_order: u32, resting_orders: u32 },
}

impl Default for Undo {
    fn default() -> Self {
        Self::Bits(0, 0)
    }
}

/// Work a fill leaves for the end of its unit, as `(kind, a, b)`. Sorting gives the order of E-13.4 and E-14.3.
pub type Mark = (u8, u64, u64);
/// `(MARK_PULL, accountId, eventId)`: maker auto-pull (E-13.2).
pub const MARK_PULL: u8 = 0;
/// `(MARK_TRIGGER, accountId, clientOrderGroup)`: an order group trigger (E-13.3).
pub const MARK_TRIGGER: u8 = 1;
/// `(MARK_FILLED, eventId, accountId)`: a set burn candidate (E-14.3).
pub const MARK_FILLED: u8 = 2;

pub type Slot<K, V> = Option<(K, V)>;
pub type HoldingKey = (u64, u64);
pub type LevelKey = (u64, u8, u16);
/// `(accountId, eventId)`.
pub type EventCashKey = (u64, u64);

/// How many of each record the engine holds. Maps get twice as many slots, rounded up to a power of two.
#[derive(Clone, Copy, Debug)]
pub struct Capacities {
    pub tick_tables: usize,
    pub tick_rows: usize,
    pub events: usize,
    pub instruments: usize,
    /// Price levels across all books, both sides: twice the sum of every book's tick table length.
    pub levels: usize,
    pub accounts: usize,
    pub holdings: usize,
    pub orders: usize,
    pub duplicates: usize,
    pub commands: usize,
    pub order_groups: usize,
    pub maker_settings: usize,
    pub maker_counters: usize,
    /// Accounts' net cash in each unresolved event they have traded (D-1).
    pub event_cash: usize,
    /// Fee overrides in force at once, expired ones not counted (D-27).
    pub fee_overrides: usize,
    /// Positions, sleeves and levels one unit may touch; also the end-of-unit marks.
    pub touches: usize,
    /// Writes one dry run may log.
    pub undo: usize,
}

impl Capacities {
    /// Map slots for `n` entries at half load.
    pub const fn slots(n: usize) -> usize {
        (n * 2).next_power_of_two()
    }

    /// Generous capacities for tests and local environments.
    pub const fn small() -> Self {
        Self {
            tick_tables: 16,
            tick_rows: 4_096,
            events: 1_024,
            instruments: 1_024,
            levels: 131_072,
            accounts: 4_096,
            holdings: 16_384,
            orders: 16_384,
            // Two hours of client order IDs (E-9.2, D-12) at about thirty-six new orders a second, where a
            // 24-hour window afforded three and a mini venue's synthetic taker alone approached it. The table
            // never evicts, so a full one fail-stops: size it as the window times new orders a second.
            duplicates: 262_144,
            commands: 65_536,
            order_groups: 4_096,
            maker_settings: 4_096,
            maker_counters: 16_384,
            event_cash: 16_384,
            fee_overrides: 1_024,
            touches: 65_536,
            undo: 65_536,
        }
    }

    /// A first production group: a quarter of a million resting orders and accounts, about 1.5 GiB.
    ///
    /// Every size is a power of two, so no map pays for rounding: a map of `n` entries gets exactly `2n` slots.
    /// `touches` is four times `orders`, as it is in [`Self::small`]: one message can fill against every resting
    /// order, and with repeated keys skipped (`unit.rs`, `orders.rs`) each fill adds at most a few records for an
    /// account not already among the last few. `undo` stays small, because only the dry-run oracle uses it.
    pub const fn launch() -> Self {
        Self {
            tick_tables: 64,
            tick_rows: 16_384,
            events: 4_096,
            instruments: 2_048,
            levels: 524_288,
            accounts: 262_144,
            holdings: 2_097_152,
            orders: 262_144,
            duplicates: 4_194_304,
            commands: 1_048_576,
            order_groups: 65_536,
            maker_settings: 65_536,
            maker_counters: 262_144,
            event_cash: 1_048_576,
            fee_overrides: 16_384,
            touches: 1_048_576,
            undo: 65_536,
        }
    }

    /// A million resting orders and accounts, about 6 GiB. Sized the same way as [`Self::launch`].
    pub const fn scale() -> Self {
        Self {
            tick_tables: 64,
            tick_rows: 65_536,
            events: 16_384,
            instruments: 8_192,
            levels: 2_097_152,
            accounts: 1_048_576,
            holdings: 8_388_608,
            orders: 1_048_576,
            duplicates: 16_777_216,
            commands: 4_194_304,
            order_groups: 262_144,
            maker_settings: 262_144,
            maker_counters: 1_048_576,
            event_cash: 4_194_304,
            fee_overrides: 65_536,
            touches: 4_194_304,
            undo: 65_536,
        }
    }

    /// The profile a node was started with, by name: `small`, `launch` or `scale`.
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "small" => Some(Self::small()),
            "launch" => Some(Self::launch()),
            "scale" => Some(Self::scale()),
            _ => None,
        }
    }
}

/// Every table the engine uses. Map slices must have [`Capacities::slots`] entries; pools have exactly the
/// capacity. Contents are overwritten at construction.
pub struct Memory<'a> {
    pub tick_tables: &'a mut [Slot<u32, TickTable>],
    pub tick_rows: &'a mut [TickRow],
    pub events: &'a mut [Slot<u64, Event>],
    pub instruments: &'a mut [Instrument],
    pub instrument_index: &'a mut [Slot<u64, u32>],
    pub levels: &'a mut [Level],
    /// Occupancy words: `levels / 64` rounded up, plus one per book side.
    pub level_bits: &'a mut [u64],
    pub accounts: &'a mut [Account],
    pub account_index: &'a mut [Slot<u64, u32>],
    pub holdings: &'a mut [Slot<HoldingKey, Holding>],
    pub orders: &'a mut [Order],
    pub order_index: &'a mut [Slot<u64, u32>],
    pub duplicates: &'a mut [Slot<(u64, ClientOrderId), Duplicate>],
    /// One `(expiresAt, key)` record per recorded client order ID, so E-8.2 removes expired entries from the front
    /// instead of scanning the table: a queue in recording order, or a min-heap by expiry while entries recorded
    /// under a longer window outlive a shortened one. At least `duplicates` slots: a full table and a tick's
    /// recordings must both fit.
    pub expiry: &'a mut [(u64, (u64, ClientOrderId))],
    /// An `(expiresAt, orderId)` min-heap of the resting orders that expire, so E-8.1 takes them from the front
    /// instead of walking every order each tick. Twice `orders`: an order cancelled or filled before it expires
    /// leaves its record behind, and since the live records can never outnumber `orders`, compacting when the heap
    /// is full always at least halves it.
    pub gtd: &'a mut [(u64, u64)],
    /// `commandId` to the expiry it was recorded with (E-7.1, D-25).
    pub commands: &'a mut [Slot<CommandId, u64>],
    /// One `(expiresAt, commandId)` record per recorded command ID, oldest first, as `expiry` is for client order
    /// IDs. At least `commands` slots. The window is fixed, so recording order is expiry order.
    pub command_expiry: &'a mut [(u64, CommandId)],
    pub order_groups: &'a mut [Slot<(u64, u32), OrderGroup>],
    pub maker_settings: &'a mut [Slot<HoldingKey, MakerSetting>],
    pub maker_counters: &'a mut [Slot<HoldingKey, WindowCounter>],
    pub event_cash: &'a mut [Slot<EventCashKey, i64>],
    /// `fee_overrides` entries, kept sorted (`fees.rs`).
    pub fee_overrides: &'a mut [FeeOverride],
    pub position_touches: &'a mut [(HoldingKey, u32, i64)],
    pub sleeve_touches: &'a mut [(u64, u32, (i64, i64))],
    pub level_touches: &'a mut [(LevelKey, u32, (u64, u32))],
    /// Working space for collecting keys to act on in order; as long as `orders`, `instruments`, half the `holdings`
    /// slots and half the `event_cash` slots.
    pub scratch: &'a mut [(u64, u64)],
    /// `touches` entries.
    pub marks: &'a mut [Mark],
    /// `undo` entries.
    pub undo: &'a mut [Undo],
    /// One per order slot: the fill check's view of each order (E-10.9), so it never writes the book. At least as
    /// long as `orders`.
    pub check_orders: &'a mut [CheckOrder],
    /// One per account slot: the fill check's view of each account's holding. At least as long as `accounts`.
    pub check_holdings: &'a mut [CheckHolding],
}
