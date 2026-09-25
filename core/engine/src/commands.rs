//! Reference data and service commands (spec §6, §7, §16).

use crate::emit::{CommandId, CommandOutcome, MarketState, Out, RejectReason};
use crate::state::*;
use crate::{fees, Engine, FailStop, ResultSink, PROTOCOL_VERSION};
use venue_journal::message_header_codec::MessageHeaderDecoder;
use venue_journal::*;

type Decoded<T> = Result<T, FailStop>;

/// A command's outcome before it is emitted.
enum Verdict {
    Applied,
    Rejected(RejectReason),
}

use Verdict::{Applied, Rejected};

/// Commands whose only rejection is `INVALID_COMMAND`.
const fn valid(ok: bool) -> Verdict {
    if ok {
        Applied
    } else {
        Rejected(RejectReason::INVALID_COMMAND)
    }
}

pub(crate) fn header(input: &[u8]) -> MessageHeaderDecoder<ReadBuf<'_>> {
    MessageHeaderDecoder::default().wrap(ReadBuf::new(input), 0)
}

#[inline]
fn malformed<T>(_: T) -> FailStop {
    FailStop::MalformedCommand
}

/// Every command's `commandId` follows its record header (18 bytes) at the start of the block.
/// How long a command ID is remembered: seven days, fixed (E-7.1, D-25).
const COMMAND_ID_WINDOW_NS: u64 = 604_800 * 1_000_000_000;

fn command_id(input: &[u8]) -> CommandId {
    let at = message_header_codec::ENCODED_LENGTH + 18;
    let mut id = [0u8; 16];
    id.copy_from_slice(&input[at..at + 16]);
    id
}

const MARKET_TRANSITIONS: [(MarketState, MarketState); 7] = [
    (MarketState::LISTED, MarketState::OPEN),
    (MarketState::OPEN, MarketState::HALTED),
    (MarketState::HALTED, MarketState::OPEN),
    (MarketState::OPEN, MarketState::CLOSED),
    (MarketState::HALTED, MarketState::CLOSED),
    (MarketState::CLOSED, MarketState::RESOLVING),
    (MarketState::RESOLVING, MarketState::CLOSED),
];

impl<'a> Engine<'a> {
    /// E-7.1: duplicate detection, the command body, its outcome and the end of the unit.
    pub(crate) fn on_command<S: ResultSink>(
        &mut self,
        template: u16,
        input: &[u8],
        sink: &mut S,
    ) -> Result<(), FailStop> {
        let id = command_id(input);
        if self.commands.get(&id).is_some_and(|&expires_at| self.now < expires_at) {
            self.emitter.emit(
                sink,
                &Out::CommandApplied {
                    command_id: id,
                    outcome: CommandOutcome::DUPLICATE,
                    reject_reason: RejectReason::NONE,
                },
            )?;
            return self.end_unit(sink);
        }
        let expires_at = self.now.checked_add(COMMAND_ID_WINDOW_NS).ok_or(FailStop::Overflow)?;
        self.record_command(id, expires_at)?;
        let verdict = match template {
            define_tick_table_codec::SBE_TEMPLATE_ID => self.check_tick_table(input)?,
            define_event_codec::SBE_TEMPLATE_ID => self.check_event(input)?,
            list_instrument_codec::SBE_TEMPLATE_ID => self.check_listing(input)?,
            sleeve_adjust_codec::SBE_TEMPLATE_ID => self.check_sleeve_adjust(input)?,
            set_group_parameters_codec::SBE_TEMPLATE_ID => self.check_group_parameters(input)?,
            set_listing_parameters_codec::SBE_TEMPLATE_ID => {
                // E-7.12
                let dec = set_listing_parameters_codec::SetListingParametersDecoder::default().header(header(input), 0);
                valid(dec.settled_retention_seconds() != 0)
            }
            activate_protocol_codec::SBE_TEMPLATE_ID => self.check_activation(input)?,
            set_market_state_codec::SBE_TEMPLATE_ID => self.check_market_state(input)?,
            set_event_phase_codec::SBE_TEMPLATE_ID => self.check_event_phase(input)?,
            set_venue_mode_codec::SBE_TEMPLATE_ID => self.check_venue_mode(input)?,
            set_rule_limits_codec::SBE_TEMPLATE_ID
            | set_account_controls_codec::SBE_TEMPLATE_ID
            | set_maker_protection_codec::SBE_TEMPLATE_ID => self.check_account_settings(template, input)?,
            set_master_account_codec::SBE_TEMPLATE_ID => self.check_master_account(input)?,
            set_fee_schedule_codec::SBE_TEMPLATE_ID => {
                let dec = set_fee_schedule_codec::SetFeeScheduleDecoder::default().header(header(input), 0);
                valid(fees::rates_valid(dec.maker_rate_micros(), dec.taker_rate_micros()))
            }
            set_fee_override_codec::SBE_TEMPLATE_ID => {
                // E-7.10: an override that has already expired is refused rather than stored unseen.
                let dec = set_fee_override_codec::SetFeeOverrideDecoder::default().header(header(input), 0);
                let unexpired = dec.expires_at().is_none_or(|at| at > self.now);
                valid(fees::rates_valid(dec.maker_rate_micros(), dec.taker_rate_micros()) && unexpired)
            }
            remove_fee_override_codec::SBE_TEMPLATE_ID => {
                let dec = remove_fee_override_codec::RemoveFeeOverrideDecoder::default().header(header(input), 0);
                valid(self.fee_override_exists(dec.override_id()))
            }
            resolve_event_codec::SBE_TEMPLATE_ID => valid(self.resolution(input)?.is_some()),
            expire_parlay_codec::SBE_TEMPLATE_ID => valid(self.parlay_expiry(input)?.is_some()),
            // §19: not in v0.
            transfer_out_codec::SBE_TEMPLATE_ID | transfer_in_codec::SBE_TEMPLATE_ID => {
                Rejected(RejectReason::INVALID_COMMAND)
            }
            _ => return Err(FailStop::NotImplemented),
        };
        match verdict {
            Rejected(reason) => {
                self.emitter.emit(
                    sink,
                    &Out::CommandApplied { command_id: id, outcome: CommandOutcome::REJECTED, reject_reason: reason },
                )?;
            }
            Applied => {
                self.emitter.emit(
                    sink,
                    &Out::CommandApplied {
                        command_id: id,
                        outcome: CommandOutcome::APPLIED,
                        reject_reason: RejectReason::NONE,
                    },
                )?;
                self.apply_command(template, input, sink)?;
            }
        }
        self.end_unit(sink)
    }

    /// The effects of a command that passed its checks.
    fn apply_command<S: ResultSink>(&mut self, template: u16, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        match template {
            define_tick_table_codec::SBE_TEMPLATE_ID => self.apply_tick_table(input),
            define_event_codec::SBE_TEMPLATE_ID => self.apply_event(input),
            list_instrument_codec::SBE_TEMPLATE_ID => self.apply_listing(input, sink),
            sleeve_adjust_codec::SBE_TEMPLATE_ID => self.apply_sleeve_adjust(input),
            set_group_parameters_codec::SBE_TEMPLATE_ID => {
                let dec = set_group_parameters_codec::SetGroupParametersDecoder::default().header(header(input), 0);
                self.active_order_limit = dec.active_order_limit();
                let window = dec.client_order_id_window_seconds();
                if window < self.client_order_id_window_seconds {
                    self.expiry_window_shortened();
                }
                self.client_order_id_window_seconds = window;
                Ok(())
            }
            set_listing_parameters_codec::SBE_TEMPLATE_ID => {
                // E-7.12: the retention applies at the next tick to every event awaiting release (E-8.4); a limit
                // applies to later listings only.
                let dec = set_listing_parameters_codec::SetListingParametersDecoder::default().header(header(input), 0);
                self.settled_retention_seconds = dec.settled_retention_seconds();
                self.max_events = dec.max_events();
                self.max_instruments = dec.max_instruments();
                self.max_price_levels = dec.max_price_levels();
                Ok(())
            }
            activate_protocol_codec::SBE_TEMPLATE_ID => {
                let dec = activate_protocol_codec::ActivateProtocolDecoder::default().header(header(input), 0);
                self.protocol_version = dec.protocol_version();
                Ok(())
            }
            set_market_state_codec::SBE_TEMPLATE_ID => self.apply_market_state(input, sink),
            set_event_phase_codec::SBE_TEMPLATE_ID => self.apply_event_phase(input, sink),
            set_venue_mode_codec::SBE_TEMPLATE_ID => self.apply_venue_mode(input, sink),
            resolve_event_codec::SBE_TEMPLATE_ID => self.apply_resolution(input, sink),
            expire_parlay_codec::SBE_TEMPLATE_ID => self.apply_parlay_expiry(input, sink),
            set_fee_schedule_codec::SBE_TEMPLATE_ID => {
                // E-7.9
                let dec = set_fee_schedule_codec::SetFeeScheduleDecoder::default().header(header(input), 0);
                self.fee_schedule =
                    FeeRates { maker_micros: dec.maker_rate_micros(), taker_micros: dec.taker_rate_micros() };
                Ok(())
            }
            set_fee_override_codec::SBE_TEMPLATE_ID => {
                // E-7.10
                let dec = set_fee_override_codec::SetFeeOverrideDecoder::default().header(header(input), 0);
                self.set_fee_override(FeeOverride {
                    account_id: dec.account_id(),
                    priority: dec.priority(),
                    id: dec.override_id(),
                    instrument_id: dec.instrument_id(),
                    event_id: dec.event_id(),
                    maker_rate_micros: dec.maker_rate_micros(),
                    taker_rate_micros: dec.taker_rate_micros(),
                    expires_at: dec.expires_at(),
                })
            }
            remove_fee_override_codec::SBE_TEMPLATE_ID => {
                // E-7.11
                let dec = remove_fee_override_codec::RemoveFeeOverrideDecoder::default().header(header(input), 0);
                self.remove_fee_override(dec.override_id());
                Ok(())
            }
            set_master_account_codec::SBE_TEMPLATE_ID => self.apply_master_account(input),
            _ => self.apply_account_settings(template, input),
        }
    }

    // ------------------------------------------------------------------ E-6.1 tick tables

    fn check_tick_table(&mut self, input: &[u8]) -> Decoded<Verdict> {
        let dec = define_tick_table_codec::DefineTickTableDecoder::default().header(header(input), 0);
        if self.tick_tables.contains(&dec.tick_table_id()) {
            return Ok(Rejected(RejectReason::INVALID_COMMAND));
        }
        let mut rows = dec.ticks_decoder();
        let len = usize::from(rows.count());
        if len == 0 {
            return Ok(Rejected(RejectReason::INVALID_COMMAND));
        }
        let start = self.tick_rows_used;
        if start + len > self.tick_rows.len() {
            return Err(FailStop::Capacity);
        }
        // Stage the rows in unused pool space; apply_tick_table commits them.
        let mut n = 0usize;
        while rows.advance().map_err(malformed)?.is_some() {
            if usize::from(rows.tick()) != n {
                return Ok(Rejected(RejectReason::INVALID_COMMAND));
            }
            self.tick_rows[start + n] =
                TickRow { numerator: rows.numerator(), denominator: rows.denominator(), complement: rows.complement() };
            n += 1;
        }
        let table = &self.tick_rows[start..start + len];
        for (t, r) in table.iter().enumerate() {
            if r.numerator == 0 || r.numerator >= r.denominator {
                return Ok(Rejected(RejectReason::INVALID_COMMAND));
            }
            if let Some(next) = table.get(t + 1) {
                if u64::from(r.numerator) * u64::from(next.denominator)
                    >= u64::from(next.numerator) * u64::from(r.denominator)
                {
                    return Ok(Rejected(RejectReason::INVALID_COMMAND));
                }
            }
            let Some(c) = table.get(usize::from(r.complement)) else {
                return Ok(Rejected(RejectReason::INVALID_COMMAND));
            };
            let lhs = u128::from(r.numerator) * u128::from(c.denominator)
                + u128::from(c.numerator) * u128::from(r.denominator);
            if lhs != u128::from(r.denominator) * u128::from(c.denominator) || usize::from(c.complement) != t {
                return Ok(Rejected(RejectReason::INVALID_COMMAND));
            }
        }
        Ok(Applied)
    }

    fn apply_tick_table(&mut self, input: &[u8]) -> Result<(), FailStop> {
        let dec = define_tick_table_codec::DefineTickTableDecoder::default().header(header(input), 0);
        let id = dec.tick_table_id();
        let len = dec.ticks_decoder().count();
        let table = TickTable { start: self.tick_rows_used as u32, len };
        self.tick_tables.insert(id, table).map_err(|_| FailStop::Capacity)?;
        self.tick_rows_used += usize::from(len);
        Ok(())
    }

    // ------------------------------------------------------------------ E-6.2 events

    fn check_event(&mut self, input: &[u8]) -> Decoded<Verdict> {
        let dec = define_event_codec::DefineEventDecoder::default().header(header(input), 0);
        if self.events.contains(&dec.event_id()) || !(2..=64).contains(&dec.state_count()) {
            return Ok(Rejected(RejectReason::INVALID_COMMAND));
        }
        // E-6.2, after validity: the events the group carries, resolved ones not yet released included (E-6.5).
        if self.max_events != 0 && self.events.len() >= self.max_events as usize {
            return Ok(Rejected(RejectReason::LISTING_CAPACITY));
        }
        Ok(Applied)
    }

    fn apply_event(&mut self, input: &[u8]) -> Result<(), FailStop> {
        let dec = define_event_codec::DefineEventDecoder::default().header(header(input), 0);
        let event = Event {
            state_count: dec.state_count(),
            scheduled_start: dec.scheduled_start(),
            first_instrument: NONE,
            ..Event::default()
        };
        self.events.insert(dec.event_id(), event).map_err(|_| FailStop::Capacity)?;
        Ok(())
    }

    // ------------------------------------------------------------------ E-6.3 listings

    fn check_listing(&mut self, input: &[u8]) -> Decoded<Verdict> {
        use venue_journal::allocation_policy::AllocationPolicy;
        use venue_journal::instrument_class::InstrumentClass;
        let dec = list_instrument_codec::ListInstrumentDecoder::default().header(header(input), 0);
        let (class, policy) = (dec.instrument_class(), dec.allocation_policy());
        if class == InstrumentClass::NullVal || policy == AllocationPolicy::NullVal {
            return Err(FailStop::MalformedCommand);
        }
        let (mask, lot) = (dec.state_mask(), dec.lot_step());
        let (instrument_id, event_id, table_id) = (dec.instrument_id(), dec.event_id(), dec.tick_table_id());
        let legs = dec.legs_decoder().count();
        let reject = Ok(Rejected(RejectReason::INVALID_COMMAND));
        if self.instrument_index.contains(&instrument_id) {
            return reject;
        }
        let Some(event) = self.events.get(&event_id).copied() else { return reject };
        let Some(table) = self.tick_tables.get(&table_id).copied() else { return reject };
        if event.resolved || lot == 0 {
            return reject;
        }
        let full = if event.state_count >= 64 { u64::MAX } else { (1u64 << event.state_count) - 1 };
        if mask == 0 || mask & !full != 0 || mask == full {
            return reject;
        }
        let rows = &self.tick_rows[table.start as usize..table.start as usize + usize::from(table.len)];
        if rows.iter().any(|r| (100 * u128::from(lot) * u128::from(r.numerator)) % u128::from(r.denominator) != 0) {
            return reject;
        }
        let ok = match class {
            InstrumentClass::BINARY => event.state_count == 2 && legs == 0,
            InstrumentClass::CATEGORICAL_OUTCOME => mask.count_ones() == 1 && legs == 0,
            InstrumentClass::SCALAR_BAND => legs == 0,
            InstrumentClass::PARLAY => event.state_count == 2 && legs > 0,
            InstrumentClass::NullVal => false,
        };
        if !ok {
            return reject;
        }
        // E-6.3, after validity: the instruments and price levels the group carries (E-6.5).
        let levels = 2 * usize::from(table.len);
        let instruments_full = self.max_instruments != 0 && self.instruments_carried >= self.max_instruments as usize;
        let levels_full = self.max_price_levels != 0 && self.levels_carried + levels > self.max_price_levels as usize;
        if instruments_full || levels_full {
            return Ok(Rejected(RejectReason::LISTING_CAPACITY));
        }
        Ok(Applied)
    }

    fn apply_listing<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        let dec = list_instrument_codec::ListInstrumentDecoder::default().header(header(input), 0);
        let table = *self.tick_tables.get(&dec.tick_table_id()).ok_or(FailStop::MalformedCommand)?;
        let per_side = usize::from(table.len);
        let words = per_side.div_ceil(64);
        // A slot a release freed first, else a new one (E-18.3).
        let slot = if self.free_instrument != NONE {
            self.free_instrument as usize
        } else if self.instruments_used < self.instruments.len() {
            self.instruments_used
        } else {
            return Err(FailStop::Capacity);
        };
        // The book's levels and occupancy words go at the end of their pools; if releases have left the room in
        // holes, the pools are compacted first, so a listing never fails for room the group does not carry.
        if self.levels_used + 2 * per_side > self.levels.len()
            || self.level_bits_used + 2 * words > self.level_bits.len()
        {
            if self.levels_carried + 2 * per_side > self.levels.len()
                || self.level_bits_carried + 2 * words > self.level_bits.len()
            {
                return Err(FailStop::Capacity);
            }
            self.compact_levels()?;
        }
        let free_next = self.instruments[slot].next_in_event;
        let mut inst = Instrument {
            in_use: true,
            id: dec.instrument_id(),
            event_id: dec.event_id(),
            class: dec.instrument_class(),
            table,
            lot_step: dec.lot_step(),
            policy: dec.allocation_policy(),
            mask: dec.state_mask(),
            position_limit: dec.position_limit(),
            market_state: MarketState::LISTED,
            next_in_event: NONE,
            levels_start: self.levels_used as u32,
            bits_start: self.level_bits_used as u32,
        };
        for l in &mut self.levels[self.levels_used..self.levels_used + 2 * per_side] {
            *l = Level { quantity: 0, count: 0, head: NONE, tail: NONE };
        }
        for w in &mut self.level_bits[self.level_bits_used..self.level_bits_used + 2 * words] {
            *w = 0;
        }
        // Link into the event's instruments, ascending by ID.
        let event = self.events.get_mut(&inst.event_id).ok_or(FailStop::MalformedCommand)?;
        let mut prev = NONE;
        let mut cur = event.first_instrument;
        while cur != NONE && self.instruments[cur as usize].id < inst.id {
            prev = cur;
            cur = self.instruments[cur as usize].next_in_event;
        }
        inst.next_in_event = cur;
        if prev == NONE {
            event.first_instrument = slot as u32;
        } else {
            self.instruments[prev as usize].next_in_event = slot as u32;
        }
        event.instrument_count += 1;
        self.instruments[slot] = inst;
        self.instrument_index.insert(inst.id, slot as u32).map_err(|_| FailStop::Capacity)?;
        if slot == self.instruments_used {
            self.instruments_used += 1;
        } else {
            self.free_instrument = free_next;
        }
        self.instruments_carried += 1;
        self.levels_used += 2 * per_side;
        self.levels_carried += 2 * per_side;
        self.level_bits_used += 2 * words;
        self.level_bits_carried += 2 * words;
        self.emitter.emit(sink, &Out::MarketStateChanged { instrument_id: inst.id, state: MarketState::LISTED })
    }

    /// Moves every carried book's levels and occupancy words to the front of their pools, in pool order, closing the
    /// holes releases left (E-8.4, E-18.3). Only a listing that does not fit at the end of a pool calls this, so its
    /// cost, the used part of the pools, is paid by a command and never by a tick. No order, touch or undo record
    /// holds a level index across inputs: queues link order slots, and orders find their level from their book.
    fn compact_levels(&mut self) -> Result<(), FailStop> {
        let mut n = 0usize;
        for (slot, inst) in self.instruments[..self.instruments_used].iter().enumerate() {
            if inst.in_use {
                *self.scratch.get_mut(n).ok_or(FailStop::Capacity)? = (u64::from(inst.levels_start), slot as u64);
                n += 1;
            }
        }
        // Books take both pools in the same order, and compaction keeps it, so one order serves both.
        self.scratch[..n].sort_unstable();
        let (mut levels_at, mut bits_at) = (0usize, 0usize);
        for j in 0..n {
            let slot = self.scratch[j].1 as usize;
            let inst = self.instruments[slot];
            let (levels, words) = (2 * usize::from(inst.table.len), 2 * inst.words_per_side());
            let (from_levels, from_bits) = (inst.levels_start as usize, inst.bits_start as usize);
            self.levels.copy_within(from_levels..from_levels + levels, levels_at);
            self.level_bits.copy_within(from_bits..from_bits + words, bits_at);
            let moved = &mut self.instruments[slot];
            moved.levels_start = levels_at as u32;
            moved.bits_start = bits_at as u32;
            levels_at += levels;
            bits_at += words;
        }
        if levels_at != self.levels_carried || bits_at != self.level_bits_carried {
            return Err(FailStop::Inconsistent);
        }
        self.levels_used = levels_at;
        self.level_bits_used = bits_at;
        Ok(())
    }

    // ------------------------------------------------------------------ E-7.2 .. E-7.7

    fn check_sleeve_adjust(&mut self, input: &[u8]) -> Decoded<Verdict> {
        use venue_journal::sleeve_reason::SleeveReason;
        let dec = sleeve_adjust_codec::SleeveAdjustDecoder::default().header(header(input), 0);
        let delta = dec.delta_cents();
        match dec.reason() {
            SleeveReason::NullVal => Err(FailStop::MalformedCommand),
            _ if delta <= 0 => Ok(Rejected(RejectReason::INVALID_COMMAND)),
            SleeveReason::GRANT => Ok(Applied),
            SleeveReason::RECLAIM => {
                let slot = self.account_slot(dec.account_id())?;
                let a = &self.accounts[slot as usize];
                let available = a.balance.checked_sub(a.reserved).ok_or(FailStop::Overflow)?;
                Ok(if delta > available.max(0) { Rejected(RejectReason::INSUFFICIENT_SLEEVE) } else { Applied })
            }
        }
    }

    fn apply_sleeve_adjust(&mut self, input: &[u8]) -> Result<(), FailStop> {
        use venue_journal::sleeve_reason::SleeveReason;
        let dec = sleeve_adjust_codec::SleeveAdjustDecoder::default().header(header(input), 0);
        let slot = self.account_slot(dec.account_id())?;
        let delta = if dec.reason() == SleeveReason::RECLAIM { -dec.delta_cents() } else { dec.delta_cents() };
        self.touch_sleeve(slot)?;
        let a = &mut self.accounts[slot as usize];
        a.balance = a.balance.checked_add(delta).ok_or(FailStop::Overflow)?;
        Ok(())
    }

    fn check_group_parameters(&mut self, input: &[u8]) -> Decoded<Verdict> {
        let dec = set_group_parameters_codec::SetGroupParametersDecoder::default().header(header(input), 0);
        Ok(if dec.client_order_id_window_seconds() == 0 { Rejected(RejectReason::INVALID_COMMAND) } else { Applied })
    }

    fn check_activation(&mut self, input: &[u8]) -> Decoded<Verdict> {
        let dec = activate_protocol_codec::ActivateProtocolDecoder::default().header(header(input), 0);
        let v = dec.protocol_version();
        if v <= self.protocol_version {
            return Ok(Rejected(RejectReason::INVALID_COMMAND));
        }
        if v > PROTOCOL_VERSION || dec.required_binary_hash() != self.config.binary_hash {
            return Err(FailStop::UnsupportedProtocol);
        }
        Ok(Applied)
    }

    fn check_account_settings(&mut self, template: u16, input: &[u8]) -> Decoded<Verdict> {
        use venue_journal::boolean_type::BooleanType;
        use venue_journal::self_match_prevention::SelfMatchPrevention;
        if template == set_account_controls_codec::SBE_TEMPLATE_ID {
            let dec = set_account_controls_codec::SetAccountControlsDecoder::default().header(header(input), 0);
            if dec.closed_only() == BooleanType::NullVal || dec.self_match_prevention() == SelfMatchPrevention::NullVal
            {
                return Err(FailStop::MalformedCommand);
            }
        }
        Ok(Applied)
    }

    fn apply_account_settings(&mut self, template: u16, input: &[u8]) -> Result<(), FailStop> {
        use venue_journal::boolean_type::BooleanType;
        match template {
            set_rule_limits_codec::SBE_TEMPLATE_ID => {
                let dec = set_rule_limits_codec::SetRuleLimitsDecoder::default().header(header(input), 0);
                let limits = RuleLimits {
                    max_resting_orders: dec.max_resting_orders(),
                    max_new_orders_per_second: dec.max_new_orders_per_second(),
                    max_order_to_trade_ratio_milli: dec.max_order_to_trade_ratio_milli(),
                    ratio_free_orders_per_second: dec.ratio_free_orders_per_second(),
                };
                if dec.account_id() == 0 {
                    self.default_limits = Some(limits);
                } else {
                    let slot = self.account_slot(dec.account_id())?;
                    self.accounts[slot as usize].limits = Some(limits);
                }
            }
            set_account_controls_codec::SBE_TEMPLATE_ID => {
                let dec = set_account_controls_codec::SetAccountControlsDecoder::default().header(header(input), 0);
                let slot = self.account_slot(dec.account_id())?;
                let a = &mut self.accounts[slot as usize];
                a.closed_only = dec.closed_only() == BooleanType::TRUE;
                a.self_match_prevention = dec.self_match_prevention();
            }
            set_maker_protection_codec::SBE_TEMPLATE_ID => {
                let dec = set_maker_protection_codec::SetMakerProtectionDecoder::default().header(header(input), 0);
                let key = (dec.account_id(), dec.event_id());
                if dec.max_contracts_filled_per_window() == 0 {
                    self.maker_settings.remove(&key);
                } else {
                    let setting = MakerSetting {
                        threshold: dec.max_contracts_filled_per_window(),
                        window_millis: dec.window_millis(),
                    };
                    self.maker_settings.insert(key, setting).map_err(|_| FailStop::Capacity)?;
                }
            }
            _ => return Err(FailStop::NotImplemented),
        }
        Ok(())
    }

    /// E-7.13. Reads with `account_slot`/`master_account_id` rather than through `account_slot(&mut self, ..)`
    /// where possible so a rejected command allocates no slot beyond what E-3.2 already treats as unobservable.
    fn check_master_account(&mut self, input: &[u8]) -> Decoded<Verdict> {
        let dec = set_master_account_codec::SetMasterAccountDecoder::default().header(header(input), 0);
        let (account_id, master_id) = (dec.account_id(), dec.master_account_id());
        if account_id == 0 || master_id == 0 || account_id == master_id {
            return Ok(Rejected(RejectReason::INVALID_COMMAND));
        }
        let master_slot = self.account_slot(master_id)?;
        if self.accounts[master_slot as usize].master_account_id != 0 {
            // A master may not itself be a subaccount: a family is two levels deep at most (D-33).
            return Ok(Rejected(RejectReason::INVALID_COMMAND));
        }
        let slot = self.account_slot(account_id)?;
        let existing = self.accounts[slot as usize].master_account_id;
        Ok(valid(existing == 0 || existing == master_id))
    }

    fn apply_master_account(&mut self, input: &[u8]) -> Result<(), FailStop> {
        let dec = set_master_account_codec::SetMasterAccountDecoder::default().header(header(input), 0);
        let slot = self.account_slot(dec.account_id())?;
        self.accounts[slot as usize].master_account_id = dec.master_account_id();
        Ok(())
    }

    // ------------------------------------------------------------------ E-16.1 .. E-16.4

    fn check_market_state(&mut self, input: &[u8]) -> Decoded<Verdict> {
        let dec = set_market_state_codec::SetMarketStateDecoder::default().header(header(input), 0);
        let to = dec.state();
        if to == MarketState::NullVal {
            return Err(FailStop::MalformedCommand);
        }
        let Some(&slot) = self.instrument_index.get(&dec.instrument_id()) else {
            return Ok(Rejected(RejectReason::INVALID_COMMAND));
        };
        let inst = self.instruments[slot as usize];
        let resolved = self.events.get(&inst.event_id).is_none_or(|e| e.resolved);
        if resolved || !MARKET_TRANSITIONS.contains(&(inst.market_state, to)) {
            return Ok(Rejected(RejectReason::INVALID_COMMAND));
        }
        Ok(Applied)
    }

    fn apply_market_state<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        let dec = set_market_state_codec::SetMarketStateDecoder::default().header(header(input), 0);
        let slot = *self.instrument_index.get(&dec.instrument_id()).ok_or(FailStop::MalformedCommand)?;
        let to = dec.state();
        if to == MarketState::CLOSED {
            self.cancel_instrument_orders(slot, crate::emit::CancelReason::MARKET_CLOSED, sink)?;
        }
        let inst = &mut self.instruments[slot as usize];
        inst.market_state = to;
        let id = inst.id;
        self.emitter.emit(sink, &Out::MarketStateChanged { instrument_id: id, state: to })
    }

    fn check_event_phase(&mut self, input: &[u8]) -> Decoded<Verdict> {
        use venue_journal::event_phase::EventPhase;
        use venue_journal::phase_source::PhaseSource;
        let dec = set_event_phase_codec::SetEventPhaseDecoder::default().header(header(input), 0);
        let phase = dec.phase();
        if phase == EventPhase::NullVal || dec.source() == PhaseSource::NullVal {
            return Err(FailStop::MalformedCommand);
        }
        let reject = Ok(Rejected(RejectReason::INVALID_COMMAND));
        let Some(event) = self.events.get(&dec.event_id()) else { return reject };
        let invalid = event.resolved
            || phase == EventPhase::SCHEDULED
            || (phase == EventPhase::STARTED && event.started)
            || (phase == EventPhase::RESUMED && !event.suspended);
        if invalid {
            return reject;
        }
        Ok(Applied)
    }

    fn apply_event_phase<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        use venue_journal::event_phase::EventPhase;
        let dec = set_event_phase_codec::SetEventPhaseDecoder::default().header(header(input), 0);
        let event_id = dec.event_id();
        let event = self.events.get_mut(&event_id).ok_or(FailStop::MalformedCommand)?;
        match dec.phase() {
            EventPhase::STARTED => {
                event.started = true;
                self.cancel_at_event_start(event_id, sink)?;
            }
            EventPhase::SUSPENDED => event.suspended = true,
            EventPhase::RESUMED => event.suspended = false,
            EventPhase::ENDED => event.ended = true,
            _ => return Err(FailStop::MalformedCommand),
        }
        Ok(())
    }

    fn check_venue_mode(&mut self, input: &[u8]) -> Decoded<Verdict> {
        use venue_journal::venue_mode::VenueMode;
        let dec = set_venue_mode_codec::SetVenueModeDecoder::default().header(header(input), 0);
        if dec.mode() == VenueMode::NullVal {
            return Err(FailStop::MalformedCommand);
        }
        Ok(Applied)
    }

    fn apply_venue_mode<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        use venue_journal::venue_mode::VenueMode;
        let dec = set_venue_mode_codec::SetVenueModeDecoder::default().header(header(input), 0);
        let mode = dec.mode();
        if mode == VenueMode::KILL_SWITCH {
            self.cancel_all_orders(crate::emit::CancelReason::KILL_SWITCH, sink)?;
        }
        self.venue_mode = mode;
        Ok(())
    }
}
