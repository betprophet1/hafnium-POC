#![no_std]
#![forbid(unsafe_code)]
#![allow(clippy::all)]
#![allow(non_camel_case_types)]

#![allow(ambiguous_glob_reexports)]

use ::core::{convert::TryInto};

pub mod activate_protocol_codec;
pub mod allocation_policy;
pub mod amend_order_codec;
pub mod at_event_start;
pub mod batch_entry_type;
pub mod batch_orders_codec;
pub mod book_level_changed_codec;
pub mod boolean_type;
pub mod cancel_order_codec;
pub mod cancel_reason;
pub mod collateral_released_codec;
pub mod command_applied_codec;
pub mod command_outcome;
pub mod contract;
pub mod define_event_codec;
pub mod define_tick_table_codec;
pub mod engine_started_codec;
pub mod event_phase;
pub mod event_released_codec;
pub mod expire_parlay_codec;
pub mod fill_codec;
pub mod group_size_encoding_codec;
pub mod instrument_class;
pub mod list_instrument_codec;
pub mod market_state;
pub mod market_state_changed_codec;
pub mod mass_cancel_cause;
pub mod mass_cancel_codec;
pub mod mass_cancel_scope;
pub mod message_header_codec;
pub mod new_order_codec;
pub mod order_accepted_codec;
pub mod order_amended_codec;
pub mod order_cancelled_codec;
pub mod order_flags;
pub mod order_group_updated_codec;
pub mod order_reduced_codec;
pub mod order_rejected_codec;
pub mod outcome_value;
pub mod phase_source;
pub mod position_changed_codec;
pub mod record_header_codec;
pub mod reject_reason;
pub mod release_kind;
pub mod remove_fee_override_codec;
pub mod reset_order_group_codec;
pub mod resolution_kind;
pub mod resolve_event_codec;
pub mod result_header_codec;
pub mod seed_purpose;
pub mod seed_set_codec;
pub mod self_match_prevention;
pub mod set_account_controls_codec;
pub mod set_event_phase_codec;
pub mod set_fee_override_codec;
pub mod set_fee_schedule_codec;
pub mod set_group_parameters_codec;
pub mod set_listing_parameters_codec;
pub mod set_maker_protection_codec;
pub mod set_market_state_codec;
pub mod set_order_group_codec;
pub mod set_rule_limits_codec;
pub mod set_venue_mode_codec;
pub mod settlement_payout_codec;
pub mod side;
pub mod sleeve_adjust_codec;
pub mod sleeve_changed_codec;
pub mod sleeve_reason;
pub mod time_in_force;
pub mod time_tick_codec;
pub mod transfer_in_codec;
pub mod transfer_out_codec;
pub mod var_data_encoding_codec;
pub mod venue_mode;

pub const SBE_SCHEMA_ID: u16 = 7101;
pub const SBE_SCHEMA_VERSION: u16 = 0;
pub const SBE_SEMANTIC_VERSION: &str = "0.1.0";

pub type SbeResult<T> = core::result::Result<T, SbeErr>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SbeErr {
    ParentNotSet,
}
impl core::fmt::Display for SbeErr {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl core::error::Error for SbeErr {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Either<L, R> {
    Left(L),
    Right(R),
}

pub trait Writer<'a>: Sized {
    fn get_buf_mut(&mut self) -> &mut WriteBuf<'a>;
}

pub trait Encoder<'a>: Writer<'a> {
    fn get_limit(&self) -> usize;
    fn set_limit(&mut self, limit: usize);
    fn nullify_optional_fields(&mut self) -> &mut Self {
        self
    }
}

pub trait ActingVersion {
    fn acting_version(&self) -> u16;
}

pub trait Reader<'a>: Sized {
    fn get_buf(&self) -> &ReadBuf<'a>;
}

pub trait Decoder<'a>: Reader<'a> {
    fn get_limit(&self) -> usize;
    fn set_limit(&mut self, limit: usize);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ReadBuf<'a> {
    data: &'a [u8],
}
impl<'a> Reader<'a> for ReadBuf<'a> {
    #[inline]
    fn get_buf(&self) -> &ReadBuf<'a> {
        self
    }
}
#[allow(dead_code)]
impl<'a> ReadBuf<'a> {
    #[inline]
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    #[inline]
    pub(crate) fn get_bytes_at<const N: usize>(slice: &[u8], index: usize) -> [u8; N] {
        slice[index..index+N].try_into().expect("slice with incorrect length")
    }

    #[inline]
    pub const fn get_u8_at(&self, index: usize) -> u8 {
        self.data[index]
    }

    #[inline]
    pub fn get_i8_at(&self, index: usize) -> i8 {
        i8::from_le_bytes(Self::get_bytes_at(self.data, index))
    }

    #[inline]
    pub fn get_i16_at(&self, index: usize) -> i16 {
        i16::from_le_bytes(Self::get_bytes_at(self.data, index))
    }

    #[inline]
    pub fn get_i32_at(&self, index: usize) -> i32 {
        i32::from_le_bytes(Self::get_bytes_at(self.data, index))
    }

    #[inline]
    pub fn get_i64_at(&self, index: usize) -> i64 {
        i64::from_le_bytes(Self::get_bytes_at(self.data, index))
    }

    #[inline]
    pub fn get_u16_at(&self, index: usize) -> u16 {
        u16::from_le_bytes(Self::get_bytes_at(self.data, index))
    }

    #[inline]
    pub fn get_u32_at(&self, index: usize) -> u32 {
        u32::from_le_bytes(Self::get_bytes_at(self.data, index))
    }

    #[inline]
    pub fn get_u64_at(&self, index: usize) -> u64 {
        u64::from_le_bytes(Self::get_bytes_at(self.data, index))
    }

    #[inline]
    pub fn get_f32_at(&self, index: usize) -> f32 {
        f32::from_le_bytes(Self::get_bytes_at(self.data, index))
    }

    #[inline]
    pub fn get_f64_at(&self, index: usize) -> f64 {
        f64::from_le_bytes(Self::get_bytes_at(self.data, index))
    }

    #[inline]
    pub fn get_slice_at(&self, index: usize, len: usize) -> &[u8] {
        &self.data[index..index+len]
    }

}

#[derive(Debug, Default)]
pub struct WriteBuf<'a> {
    data: &'a mut [u8],
}
impl<'a> WriteBuf<'a> {
    pub const fn new(data: &'a mut [u8]) -> Self {
        Self { data }
    }

    #[inline]
    pub fn put_bytes_at<const COUNT: usize>(&mut self, index: usize, bytes: &[u8; COUNT]) -> usize {
        self.data[index..index + COUNT].copy_from_slice(bytes);
        COUNT
    }

    #[inline]
    pub const fn put_u8_at(&mut self, index: usize, value: u8) {
        self.data[index] = value;
    }

    #[inline]
    pub fn put_i8_at(&mut self, index: usize, value: i8) {
        self.put_bytes_at(index, &i8::to_le_bytes(value));
    }

    #[inline]
    pub fn put_i16_at(&mut self, index: usize, value: i16) {
        self.put_bytes_at(index, &i16::to_le_bytes(value));
    }

    #[inline]
    pub fn put_i32_at(&mut self, index: usize, value: i32) {
        self.put_bytes_at(index, &i32::to_le_bytes(value));
    }

    #[inline]
    pub fn put_i64_at(&mut self, index: usize, value: i64) {
        self.put_bytes_at(index, &i64::to_le_bytes(value));
    }

    #[inline]
    pub fn put_u16_at(&mut self, index: usize, value: u16) {
        self.put_bytes_at(index, &u16::to_le_bytes(value));
    }

    #[inline]
    pub fn put_u32_at(&mut self, index: usize, value: u32) {
        self.put_bytes_at(index, &u32::to_le_bytes(value));
    }

    #[inline]
    pub fn put_u64_at(&mut self, index: usize, value: u64) {
        self.put_bytes_at(index, &u64::to_le_bytes(value));
    }

    #[inline]
    pub fn put_f32_at(&mut self, index: usize, value: f32) {
        self.put_bytes_at(index, &f32::to_le_bytes(value));
    }

    #[inline]
    pub fn put_f64_at(&mut self, index: usize, value: f64) {
        self.put_bytes_at(index, &f64::to_le_bytes(value));
    }

    #[inline]
    pub fn put_slice_at(&mut self, index: usize, src: &[u8]) -> usize {
        let len = src.len();
        let dest = self.data.split_at_mut(index).1.split_at_mut(len).0;
        dest.clone_from_slice(src);
        len
    }
}
impl<'a> From<&'a mut WriteBuf<'a>> for &'a mut [u8] {
    #[inline]
    fn from(buf: &'a mut WriteBuf<'a>) -> &'a mut [u8] {
        buf.data
    }
}

