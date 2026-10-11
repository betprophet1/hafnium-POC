//! Heap-backed storage for the engine's tables. The engine itself never allocates; a host allocates once.

use venue_engine::state::*;

/// Owns one allocation per table of [`Memory`], sized from [`Capacities`].
pub struct OwnedMemory {
    tick_tables: Vec<Slot<u32, TickTable>>,
    tick_rows: Vec<TickRow>,
    events: Vec<Slot<u64, Event>>,
    instruments: Vec<Instrument>,
    instrument_index: Vec<Slot<u64, u32>>,
    levels: Vec<Level>,
    level_bits: Vec<u64>,
    overflow: Vec<OverflowEntry>,
    accounts: Vec<Account>,
    account_index: Vec<Slot<u64, u32>>,
    holdings: Vec<Slot<HoldingKey, Holding>>,
    orders: Vec<Order>,
    order_index: Vec<Slot<u64, u32>>,
    duplicates: Vec<Slot<(u64, [u8; 32]), Duplicate>>,
    expiry: Vec<(u64, (u64, [u8; 32]))>,
    gtd: Vec<(u64, u64)>,
    commands: Vec<Slot<[u8; 16], u64>>,
    command_expiry: Vec<(u64, [u8; 16])>,
    order_groups: Vec<Slot<(u64, u32), OrderGroup>>,
    maker_settings: Vec<Slot<HoldingKey, MakerSetting>>,
    maker_counters: Vec<Slot<HoldingKey, WindowCounter>>,
    event_cash: Vec<Slot<EventCashKey, i64>>,
    fee_overrides: Vec<FeeOverride>,
    position_touches: Vec<(HoldingKey, u32, i64)>,
    sleeve_touches: Vec<(u64, u32, (i64, i64))>,
    level_touches: Vec<(LevelKey, u32, (u64, u32))>,
    scratch: Vec<(u64, u64)>,
    marks: Vec<Mark>,
    undo: Vec<Undo>,
    check_orders: Vec<CheckOrder>,
    check_holdings: Vec<CheckHolding>,
}

impl OwnedMemory {
    pub fn new(c: &Capacities) -> Self {
        let s = Capacities::slots;
        Self {
            tick_tables: vec![None; s(c.tick_tables)],
            tick_rows: vec![TickRow::default(); c.tick_rows],
            events: vec![None; s(c.events)],
            instruments: vec![Instrument::default(); c.instruments],
            instrument_index: vec![None; s(c.instruments)],
            levels: vec![Level::default(); c.levels],
            level_bits: vec![0; c.levels.div_ceil(64) + 2 * c.instruments],
            overflow: vec![OverflowEntry::default(); c.overflow],
            accounts: vec![Account::default(); c.accounts],
            account_index: vec![None; s(c.accounts)],
            holdings: vec![None; s(c.holdings)],
            orders: vec![Order::default(); c.orders],
            order_index: vec![None; s(c.orders)],
            duplicates: vec![None; s(c.duplicates)],
            expiry: vec![(0, (0, [0; 32])); s(c.duplicates)],
            gtd: vec![(0, 0); c.orders * 2],
            commands: vec![None; s(c.commands)],
            command_expiry: vec![(0, [0; 16]); s(c.commands)],
            order_groups: vec![None; s(c.order_groups)],
            maker_settings: vec![None; s(c.maker_settings)],
            maker_counters: vec![None; s(c.maker_counters)],
            event_cash: vec![None; s(c.event_cash)],
            fee_overrides: vec![FeeOverride::default(); c.fee_overrides],
            position_touches: vec![((0, 0), 0, 0); c.touches],
            sleeve_touches: vec![(0, 0, (0, 0)); c.touches],
            level_touches: vec![((0, 0, 0), 0, (0, 0)); c.touches],
            // The engine requires the first four (`scratch_needed`); the snapshot writer also copies the whole
            // command-ID window and the duplicate table through it, so those are sized in as well. Without them a
            // venue whose window had filled past the order count panicked taking a snapshot (index out of bounds),
            // with nothing in the engine's own check to say why. A working buffer: the snapshot bytes do not
            // depend on its size.
            scratch: vec![
                (0, 0);
                c.orders
                    .max(c.instruments)
                    .max(s(c.holdings) / 2)
                    .max(s(c.event_cash) / 2)
                    .max(c.commands)
                    .max(c.duplicates)
            ],
            marks: vec![(0, 0, 0); c.touches],
            undo: vec![Undo::default(); c.undo],
            check_orders: vec![CheckOrder::default(); c.orders],
            check_holdings: vec![CheckHolding::default(); c.accounts],
        }
    }

    /// Lends the tables to an engine.
    pub fn memory(&mut self) -> Memory<'_> {
        Memory {
            tick_tables: &mut self.tick_tables,
            tick_rows: &mut self.tick_rows,
            events: &mut self.events,
            instruments: &mut self.instruments,
            instrument_index: &mut self.instrument_index,
            levels: &mut self.levels,
            level_bits: &mut self.level_bits,
            overflow: &mut self.overflow,
            accounts: &mut self.accounts,
            account_index: &mut self.account_index,
            holdings: &mut self.holdings,
            orders: &mut self.orders,
            order_index: &mut self.order_index,
            duplicates: &mut self.duplicates,
            expiry: &mut self.expiry,
            gtd: &mut self.gtd,
            commands: &mut self.commands,
            command_expiry: &mut self.command_expiry,
            order_groups: &mut self.order_groups,
            maker_settings: &mut self.maker_settings,
            maker_counters: &mut self.maker_counters,
            event_cash: &mut self.event_cash,
            fee_overrides: &mut self.fee_overrides,
            position_touches: &mut self.position_touches,
            sleeve_touches: &mut self.sleeve_touches,
            level_touches: &mut self.level_touches,
            scratch: &mut self.scratch,
            marks: &mut self.marks,
            undo: &mut self.undo,
            check_orders: &mut self.check_orders,
            check_holdings: &mut self.check_holdings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use venue_engine::{Engine, EngineConfig, FailStop};

    /// A table's name and a way to leave it shorter than its rule allows.
    type Shorten = (&'static str, fn(&mut OwnedMemory));

    fn build(owned: &mut OwnedMemory) -> Result<(), FailStop> {
        let config = EngineConfig { sequencer_group: 1, binary_hash: [0; 32], table_seed: 7 };
        Engine::new(config, owned.memory()).map(|_| ())
    }

    #[test]
    fn tables_sized_by_their_rules_build() {
        // The larger profiles take seconds to allocate in a debug build; the launch stress runs build them.
        assert_eq!(build(&mut OwnedMemory::new(&Capacities::small())), Ok(()));
    }

    /// The snapshot writer copies the whole command-ID window and duplicate table through the scratch array, and
    /// the engine's own size check does not ask for room for them. In the small profile the window is four times
    /// the order count, so a scratch sized only by the engine's rule overflowed once enough commands had been
    /// seen: index 16384 of 16384, taking a snapshot of a live venue.
    #[test]
    fn scratch_has_room_for_every_entry_the_snapshot_writer_copies_through_it() {
        let c = Capacities::small();
        let owned = OwnedMemory::new(&c);
        assert!(owned.scratch.len() >= c.commands, "command window: {} < {}", owned.scratch.len(), c.commands);
        assert!(owned.scratch.len() >= c.duplicates, "duplicates: {} < {}", owned.scratch.len(), c.duplicates);
    }

    #[test]
    fn a_table_shorter_than_its_rule_is_refused_before_the_first_input() {
        let cases: [Shorten; 5] = [
            ("gtd", |o| {
                o.gtd.pop();
            }),
            ("expiry", |o| {
                o.expiry.pop();
            }),
            ("command_expiry", |o| {
                o.command_expiry.pop();
            }),
            ("check_orders", |o| {
                o.check_orders.pop();
            }),
            ("scratch", |o| o.scratch.truncate(0)),
        ];
        for (table, shorten) in cases {
            let mut owned = OwnedMemory::new(&Capacities::small());
            shorten(&mut owned);
            assert_eq!(build(&mut owned), Err(FailStop::Capacity), "{table}");
        }
    }
}
