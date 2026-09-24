use crate::*;

pub use decoder::FillDecoder;
pub use encoder::FillEncoder;

pub use crate::SBE_SCHEMA_ID;
pub use crate::SBE_SCHEMA_VERSION;
pub use crate::SBE_SEMANTIC_VERSION;

pub const SBE_BLOCK_LENGTH: u16 = 125;
pub const SBE_TEMPLATE_ID: u16 = 102;

pub mod encoder {
    use super::*;
    use message_header_codec::*;

    #[derive(Debug, Default)]
    pub struct FillEncoder<'a> {
        buf: WriteBuf<'a>,
        initial_offset: usize,
        offset: usize,
        limit: usize,
    }

    impl<'a> Writer<'a> for FillEncoder<'a> {
        #[inline]
        fn get_buf_mut(&mut self) -> &mut WriteBuf<'a> {
            &mut self.buf
        }
    }

    impl<'a> Encoder<'a> for FillEncoder<'a> {
        #[inline]
        fn get_limit(&self) -> usize {
            self.limit
        }

        #[inline]
        fn set_limit(&mut self, limit: usize) {
            self.limit = limit;
        }

        /// Set all optional fields to their 'null' values.
        #[inline]
        fn nullify_optional_fields(&mut self) -> &mut Self {
            {
                let mut composite_encoder = core::mem::take(self).result_encoder();
                composite_encoder.nullify_optional_fields();
                *self = composite_encoder.parent().expect("parent missing");
            }
            self
        }
    }

    impl<'a> FillEncoder<'a> {
        pub fn wrap(mut self, buf: WriteBuf<'a>, offset: usize) -> Self {
            let limit = offset + SBE_BLOCK_LENGTH as usize;
            self.buf = buf;
            self.initial_offset = offset;
            self.offset = offset;
            self.limit = limit;
            self
        }

        #[inline]
        pub const fn encoded_length(&self) -> usize {
            self.limit - self.offset
        }

        #[inline]
        pub fn header(self, offset: usize) -> MessageHeaderEncoder<Self> {
            let mut header = MessageHeaderEncoder::default().wrap(self, offset);
            header.block_length(SBE_BLOCK_LENGTH);
            header.template_id(SBE_TEMPLATE_ID);
            header.schema_id(SBE_SCHEMA_ID);
            header.version(SBE_SCHEMA_VERSION);
            header
        }

        /// COMPOSITE ENCODER
        #[inline]
        pub fn result_encoder(self) -> result_header_codec::ResultHeaderEncoder<Self> {
            let offset = self.offset;
            result_header_codec::ResultHeaderEncoder::default().wrap(self, offset)
        }

        /// primitive field 'tradeId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 22
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn trade_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 22;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// primitive field 'instrumentId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 30
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn instrument_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 30;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// primitive field 'tick'
        /// - min value: 0
        /// - max value: 65534
        /// - null value: 0xffff_u16
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 38
        /// - encodedLength: 2
        /// - version: 0
        #[inline]
        pub fn tick(&mut self, value: u16) -> &mut Self {
            let offset = self.offset + 38;
            self.get_buf_mut().put_u16_at(offset, value);
            self
        }

        /// primitive field 'quantity'
        /// - min value: 0
        /// - max value: 4294967294
        /// - null value: 0xffffffff_u32
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 40
        /// - encodedLength: 4
        /// - version: 0
        #[inline]
        pub fn quantity(&mut self, value: u32) -> &mut Self {
            let offset = self.offset + 40;
            self.get_buf_mut().put_u32_at(offset, value);
            self
        }

        /// primitive field 'makerOrderId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 44
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn maker_order_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 44;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// primitive field 'makerAccountId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 52
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn maker_account_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 52;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// primitive field 'makerRemaining'
        /// - min value: 0
        /// - max value: 4294967294
        /// - null value: 0xffffffff_u32
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 60
        /// - encodedLength: 4
        /// - version: 0
        #[inline]
        pub fn maker_remaining(&mut self, value: u32) -> &mut Self {
            let offset = self.offset + 60;
            self.get_buf_mut().put_u32_at(offset, value);
            self
        }

        /// primitive field 'takerOrderId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 64
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn taker_order_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 64;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// primitive field 'takerAccountId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 72
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn taker_account_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 72;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// primitive field 'takerRemaining'
        /// - min value: 0
        /// - max value: 4294967294
        /// - null value: 0xffffffff_u32
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 80
        /// - encodedLength: 4
        /// - version: 0
        #[inline]
        pub fn taker_remaining(&mut self, value: u32) -> &mut Self {
            let offset = self.offset + 80;
            self.get_buf_mut().put_u32_at(offset, value);
            self
        }

        /// REQUIRED enum
        #[inline]
        pub fn taker_side(&mut self, value: side::Side) -> &mut Self {
            let offset = self.offset + 84;
            self.get_buf_mut().put_u8_at(offset, value as u8);
            self
        }

        /// primitive field 'mintedPairs'
        /// - description: New YES and NO pairs created; open interest rises by mintedPairs - burnedPairs
        /// - min value: 0
        /// - max value: 4294967294
        /// - null value: 0xffffffff_u32
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 85
        /// - encodedLength: 4
        /// - version: 0
        #[inline]
        pub fn minted_pairs(&mut self, value: u32) -> &mut Self {
            let offset = self.offset + 85;
            self.get_buf_mut().put_u32_at(offset, value);
            self
        }

        /// primitive field 'burnedPairs'
        /// - description: Complete sets destroyed
        /// - min value: 0
        /// - max value: 4294967294
        /// - null value: 0xffffffff_u32
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 89
        /// - encodedLength: 4
        /// - version: 0
        #[inline]
        pub fn burned_pairs(&mut self, value: u32) -> &mut Self {
            let offset = self.offset + 89;
            self.get_buf_mut().put_u32_at(offset, value);
            self
        }

        /// primitive field 'buyerCashCents'
        /// - description: Change to the buyer's sleeve balance before fees; negative when paid
        /// - min value: -9223372036854775807
        /// - max value: 9223372036854775807
        /// - null value: -9223372036854775808_i64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 93
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn buyer_cash_cents(&mut self, value: i64) -> &mut Self {
            let offset = self.offset + 93;
            self.get_buf_mut().put_i64_at(offset, value);
            self
        }

        /// primitive field 'sellerCashCents'
        /// - description: Change to the seller's sleeve balance before fees; negative when paid
        /// - min value: -9223372036854775807
        /// - max value: 9223372036854775807
        /// - null value: -9223372036854775808_i64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 101
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn seller_cash_cents(&mut self, value: i64) -> &mut Self {
            let offset = self.offset + 101;
            self.get_buf_mut().put_i64_at(offset, value);
            self
        }

        /// primitive field 'makerFeeCents'
        /// - description: Taken from the maker's balance and credited to account 0; negative for a rebate paid from account 0 (spec E-10.11)
        /// - min value: -9223372036854775807
        /// - max value: 9223372036854775807
        /// - null value: -9223372036854775808_i64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 109
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn maker_fee_cents(&mut self, value: i64) -> &mut Self {
            let offset = self.offset + 109;
            self.get_buf_mut().put_i64_at(offset, value);
            self
        }

        /// primitive field 'takerFeeCents'
        /// - description: Taken from the taker's balance and credited to account 0 (spec E-10.11)
        /// - min value: -9223372036854775807
        /// - max value: 9223372036854775807
        /// - null value: -9223372036854775808_i64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 117
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn taker_fee_cents(&mut self, value: i64) -> &mut Self {
            let offset = self.offset + 117;
            self.get_buf_mut().put_i64_at(offset, value);
            self
        }

    }

} // end encoder

pub mod decoder {
    use super::*;
    use message_header_codec::*;

    #[derive(Clone, Copy, Debug, Default)]
    pub struct FillDecoder<'a> {
        buf: ReadBuf<'a>,
        initial_offset: usize,
        offset: usize,
        limit: usize,
        pub acting_block_length: u16,
        pub acting_version: u16,
    }

    impl ActingVersion for FillDecoder<'_> {
        #[inline]
        fn acting_version(&self) -> u16 {
            self.acting_version
        }
    }

    impl<'a> Reader<'a> for FillDecoder<'a> {
        #[inline]
        fn get_buf(&self) -> &ReadBuf<'a> {
            &self.buf
        }
    }

    impl<'a> Decoder<'a> for FillDecoder<'a> {
        #[inline]
        fn get_limit(&self) -> usize {
            self.limit
        }

        #[inline]
        fn set_limit(&mut self, limit: usize) {
            self.limit = limit;
        }
    }

    impl<'a> FillDecoder<'a> {
        pub fn wrap(
            mut self,
            buf: ReadBuf<'a>,
            offset: usize,
            acting_block_length: u16,
            acting_version: u16,
        ) -> Self {
            let limit = offset + acting_block_length as usize;
            self.buf = buf;
            self.initial_offset = offset;
            self.offset = offset;
            self.limit = limit;
            self.acting_block_length = acting_block_length;
            self.acting_version = acting_version;
            self
        }

        #[inline]
        pub const fn encoded_length(&self) -> usize {
            self.limit - self.offset
        }

        #[inline]
        pub fn header(self, mut header: MessageHeaderDecoder<ReadBuf<'a>>, offset: usize) -> Self {
            debug_assert_eq!(SBE_TEMPLATE_ID, header.template_id());
            let acting_block_length = header.block_length();
            let acting_version = header.version();

            self.wrap(
                header.parent().unwrap(),
                offset + message_header_codec::ENCODED_LENGTH,
                acting_block_length,
                acting_version,
            )
        }

        /// COMPOSITE DECODER
        #[inline]
        pub fn result_decoder(self) -> result_header_codec::ResultHeaderDecoder<Self> {
            let offset = self.offset;
            result_header_codec::ResultHeaderDecoder::default().wrap(self, offset)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn trade_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 22)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn instrument_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 30)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn tick(&self) -> u16 {
            self.get_buf().get_u16_at(self.offset + 38)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn quantity(&self) -> u32 {
            self.get_buf().get_u32_at(self.offset + 40)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn maker_order_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 44)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn maker_account_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 52)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn maker_remaining(&self) -> u32 {
            self.get_buf().get_u32_at(self.offset + 60)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn taker_order_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 64)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn taker_account_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 72)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn taker_remaining(&self) -> u32 {
            self.get_buf().get_u32_at(self.offset + 80)
        }

        /// REQUIRED enum
        #[inline]
        pub fn taker_side(&self) -> side::Side {
            self.get_buf().get_u8_at(self.offset + 84).into()
        }

        /// primitive field - 'REQUIRED'
        /// - description: New YES and NO pairs created; open interest rises by mintedPairs - burnedPairs
        #[inline]
        pub fn minted_pairs(&self) -> u32 {
            self.get_buf().get_u32_at(self.offset + 85)
        }

        /// primitive field - 'REQUIRED'
        /// - description: Complete sets destroyed
        #[inline]
        pub fn burned_pairs(&self) -> u32 {
            self.get_buf().get_u32_at(self.offset + 89)
        }

        /// primitive field - 'REQUIRED'
        /// - description: Change to the buyer's sleeve balance before fees; negative when paid
        #[inline]
        pub fn buyer_cash_cents(&self) -> i64 {
            self.get_buf().get_i64_at(self.offset + 93)
        }

        /// primitive field - 'REQUIRED'
        /// - description: Change to the seller's sleeve balance before fees; negative when paid
        #[inline]
        pub fn seller_cash_cents(&self) -> i64 {
            self.get_buf().get_i64_at(self.offset + 101)
        }

        /// primitive field - 'REQUIRED'
        /// - description: Taken from the maker's balance and credited to account 0; negative for a rebate paid from account 0 (spec E-10.11)
        #[inline]
        pub fn maker_fee_cents(&self) -> i64 {
            self.get_buf().get_i64_at(self.offset + 109)
        }

        /// primitive field - 'REQUIRED'
        /// - description: Taken from the taker's balance and credited to account 0 (spec E-10.11)
        #[inline]
        pub fn taker_fee_cents(&self) -> i64 {
            self.get_buf().get_i64_at(self.offset + 117)
        }

    }

} // end decoder

