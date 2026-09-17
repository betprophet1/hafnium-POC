//! Framing checks on a journal input before any generated decoder reads it (spec E-4.3).
//!
//! The generated decoders index the buffer directly and would panic on a truncated message, so every length
//! they will read is checked here first: the message header, the fixed block, each repeating group and any
//! variable-length data.

use venue_journal::message_header_codec::{self, MessageHeaderDecoder};
use venue_journal::*;

/// Who issues an input, which decides how a malformed one is handled (E-4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// From a gateway on behalf of an account: rejected `MALFORMED`.
    Client,
    /// From a service: a fail-stop.
    Command,
    /// From the sequencer or a node: a fail-stop.
    Sequencer,
}

/// A checked input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub template_id: u16,
    pub kind: Kind,
}

/// Why an input could not be decoded safely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Shorter than its header, block, groups or data require.
    Short,
    /// Not this schema.
    Schema,
    /// No input message has this template ID.
    UnknownTemplate(u16),
    /// A block or group is shorter than this engine's schema version defines.
    Layout(u16),
}

struct Layout {
    template: u16,
    kind: Kind,
    block: u16,
    group_block: Option<u16>,
    var_data: bool,
}

const fn fixed(template: u16, kind: Kind, block: u16) -> Layout {
    Layout { template, kind, block, group_block: None, var_data: false }
}

const fn grouped(template: u16, kind: Kind, block: u16, group_block: u16) -> Layout {
    Layout { template, kind, block, group_block: Some(group_block), var_data: false }
}

// Group block lengths are the generated group encoders' block_length() values.
const LAYOUTS: &[Layout] = &[
    fixed(new_order_codec::SBE_TEMPLATE_ID, Kind::Client, new_order_codec::SBE_BLOCK_LENGTH),
    fixed(cancel_order_codec::SBE_TEMPLATE_ID, Kind::Client, cancel_order_codec::SBE_BLOCK_LENGTH),
    fixed(amend_order_codec::SBE_TEMPLATE_ID, Kind::Client, amend_order_codec::SBE_BLOCK_LENGTH),
    fixed(mass_cancel_codec::SBE_TEMPLATE_ID, Kind::Client, mass_cancel_codec::SBE_BLOCK_LENGTH),
    grouped(batch_orders_codec::SBE_TEMPLATE_ID, Kind::Client, batch_orders_codec::SBE_BLOCK_LENGTH, 89),
    fixed(set_order_group_codec::SBE_TEMPLATE_ID, Kind::Client, set_order_group_codec::SBE_BLOCK_LENGTH),
    fixed(reset_order_group_codec::SBE_TEMPLATE_ID, Kind::Client, reset_order_group_codec::SBE_BLOCK_LENGTH),
    fixed(time_tick_codec::SBE_TEMPLATE_ID, Kind::Sequencer, time_tick_codec::SBE_BLOCK_LENGTH),
    fixed(engine_started_codec::SBE_TEMPLATE_ID, Kind::Sequencer, engine_started_codec::SBE_BLOCK_LENGTH),
    fixed(seed_set_codec::SBE_TEMPLATE_ID, Kind::Sequencer, seed_set_codec::SBE_BLOCK_LENGTH),
    fixed(activate_protocol_codec::SBE_TEMPLATE_ID, Kind::Command, activate_protocol_codec::SBE_BLOCK_LENGTH),
    fixed(sleeve_adjust_codec::SBE_TEMPLATE_ID, Kind::Command, sleeve_adjust_codec::SBE_BLOCK_LENGTH),
    grouped(define_tick_table_codec::SBE_TEMPLATE_ID, Kind::Command, define_tick_table_codec::SBE_BLOCK_LENGTH, 12),
    fixed(define_event_codec::SBE_TEMPLATE_ID, Kind::Command, define_event_codec::SBE_BLOCK_LENGTH),
    grouped(list_instrument_codec::SBE_TEMPLATE_ID, Kind::Command, list_instrument_codec::SBE_BLOCK_LENGTH, 17),
    fixed(set_event_phase_codec::SBE_TEMPLATE_ID, Kind::Command, set_event_phase_codec::SBE_BLOCK_LENGTH),
    fixed(set_market_state_codec::SBE_TEMPLATE_ID, Kind::Command, set_market_state_codec::SBE_BLOCK_LENGTH),
    fixed(set_venue_mode_codec::SBE_TEMPLATE_ID, Kind::Command, set_venue_mode_codec::SBE_BLOCK_LENGTH),
    grouped(resolve_event_codec::SBE_TEMPLATE_ID, Kind::Command, resolve_event_codec::SBE_BLOCK_LENGTH, 5),
    fixed(expire_parlay_codec::SBE_TEMPLATE_ID, Kind::Command, expire_parlay_codec::SBE_BLOCK_LENGTH),
    fixed(set_rule_limits_codec::SBE_TEMPLATE_ID, Kind::Command, set_rule_limits_codec::SBE_BLOCK_LENGTH),
    fixed(set_group_parameters_codec::SBE_TEMPLATE_ID, Kind::Command, set_group_parameters_codec::SBE_BLOCK_LENGTH),
    fixed(set_account_controls_codec::SBE_TEMPLATE_ID, Kind::Command, set_account_controls_codec::SBE_BLOCK_LENGTH),
    fixed(set_maker_protection_codec::SBE_TEMPLATE_ID, Kind::Command, set_maker_protection_codec::SBE_BLOCK_LENGTH),
    fixed(set_fee_schedule_codec::SBE_TEMPLATE_ID, Kind::Command, set_fee_schedule_codec::SBE_BLOCK_LENGTH),
    fixed(set_fee_override_codec::SBE_TEMPLATE_ID, Kind::Command, set_fee_override_codec::SBE_BLOCK_LENGTH),
    fixed(remove_fee_override_codec::SBE_TEMPLATE_ID, Kind::Command, remove_fee_override_codec::SBE_BLOCK_LENGTH),
    fixed(set_listing_parameters_codec::SBE_TEMPLATE_ID, Kind::Command, set_listing_parameters_codec::SBE_BLOCK_LENGTH),
    fixed(transfer_out_codec::SBE_TEMPLATE_ID, Kind::Command, transfer_out_codec::SBE_BLOCK_LENGTH),
    Layout {
        template: transfer_in_codec::SBE_TEMPLATE_ID,
        kind: Kind::Command,
        block: transfer_in_codec::SBE_BLOCK_LENGTH,
        group_block: None,
        var_data: true,
    },
];

#[inline]
fn u16_at(input: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*input.get(at)?, *input.get(at + 1)?]))
}

#[inline]
fn u32_at(input: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes([*input.get(at)?, *input.get(at + 1)?, *input.get(at + 2)?, *input.get(at + 3)?]))
}

/// The kind of an input template, if it is one.
pub fn kind_of(template_id: u16) -> Option<Kind> {
    LAYOUTS.iter().find(|l| l.template == template_id).map(|l| l.kind)
}

/// Checks that every byte the decoders will read is present and the layout is this schema's.
pub fn check(input: &[u8]) -> Result<Frame, FrameError> {
    let header_len = message_header_codec::ENCODED_LENGTH;
    if input.len() < header_len {
        return Err(FrameError::Short);
    }
    let header = MessageHeaderDecoder::default().wrap(ReadBuf::new(input), 0);
    if header.schema_id() != SBE_SCHEMA_ID {
        return Err(FrameError::Schema);
    }
    let template = header.template_id();
    let layout = LAYOUTS.iter().find(|l| l.template == template).ok_or(FrameError::UnknownTemplate(template))?;
    if header.block_length() < layout.block {
        return Err(FrameError::Layout(template));
    }
    let mut at = header_len + usize::from(header.block_length());
    if at > input.len() {
        return Err(FrameError::Short);
    }
    if let Some(group_block) = layout.group_block {
        let block = u16_at(input, at).ok_or(FrameError::Short)?;
        let count = u16_at(input, at + 2).ok_or(FrameError::Short)?;
        if block < group_block {
            return Err(FrameError::Layout(template));
        }
        at += 4 + usize::from(block) * usize::from(count);
        if at > input.len() {
            return Err(FrameError::Short);
        }
    }
    if layout.var_data {
        let len = u32_at(input, at).ok_or(FrameError::Short)?;
        at += 4 + len as usize;
        if at > input.len() {
            return Err(FrameError::Short);
        }
    }
    Ok(Frame { template_id: template, kind: layout.kind })
}

#[cfg(test)]
mod tests {
    use super::*;
    use venue_journal::time_tick_codec::TimeTickEncoder;

    #[test]
    fn accepts_a_whole_message_and_rejects_truncations() {
        let mut buf = [0u8; 64];
        let mut enc = TimeTickEncoder::default().wrap(WriteBuf::new(&mut buf), message_header_codec::ENCODED_LENGTH);
        enc = enc.header(0).parent().unwrap();
        let len = enc.get_limit();
        assert_eq!(check(&buf[..len]), Ok(Frame { template_id: 50, kind: Kind::Sequencer }));
        for cut in 0..len {
            assert_eq!(check(&buf[..cut]), Err(FrameError::Short), "cut at {cut}");
        }
    }

    #[test]
    fn rejects_other_schemas_and_unknown_templates() {
        let mut buf = [0u8; 64];
        buf[0..2].copy_from_slice(&18u16.to_le_bytes());
        buf[2..4].copy_from_slice(&50u16.to_le_bytes());
        buf[4..6].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(check(&buf[..26]), Err(FrameError::Schema));
        buf[4..6].copy_from_slice(&SBE_SCHEMA_ID.to_le_bytes());
        buf[2..4].copy_from_slice(&100u16.to_le_bytes());
        assert_eq!(check(&buf[..26]), Err(FrameError::UnknownTemplate(100)));
    }
}
