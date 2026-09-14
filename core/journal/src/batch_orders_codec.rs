use crate::*;

pub use decoder::BatchOrdersDecoder;
pub use encoder::BatchOrdersEncoder;

pub use crate::SBE_SCHEMA_ID;
pub use crate::SBE_SCHEMA_VERSION;
pub use crate::SBE_SEMANTIC_VERSION;

pub const SBE_BLOCK_LENGTH: u16 = 34;
pub const SBE_TEMPLATE_ID: u16 = 5;

pub mod encoder {
    use super::*;
    use message_header_codec::*;

    #[derive(Debug, Default)]
    pub struct BatchOrdersEncoder<'a> {
        buf: WriteBuf<'a>,
        initial_offset: usize,
        offset: usize,
        limit: usize,
    }

    impl<'a> Writer<'a> for BatchOrdersEncoder<'a> {
        #[inline]
        fn get_buf_mut(&mut self) -> &mut WriteBuf<'a> {
            &mut self.buf
        }
    }

    impl<'a> Encoder<'a> for BatchOrdersEncoder<'a> {
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
                let mut composite_encoder = core::mem::take(self).record_encoder();
                composite_encoder.nullify_optional_fields();
                *self = composite_encoder.parent().expect("parent missing");
            }
            self
        }
    }

    impl<'a> BatchOrdersEncoder<'a> {
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
        pub fn record_encoder(self) -> record_header_codec::RecordHeaderEncoder<Self> {
            let offset = self.offset;
            record_header_codec::RecordHeaderEncoder::default().wrap(self, offset)
        }

        /// primitive field 'sessionId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 18
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn session_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 18;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// primitive field 'accountId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 26
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn account_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 26;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// GROUP ENCODER (id=10)
        #[inline]
        pub fn entries_encoder(self, count: u16, entries_encoder: EntriesEncoder<Self>) -> EntriesEncoder<Self> {
            entries_encoder.wrap(self, count)
        }

    }

    #[derive(Debug, Default)]
    pub struct EntriesEncoder<P> {
        parent: Option<P>,
        count: u16,
        index: usize,
        offset: usize,
        initial_limit: usize,
    }

    impl<'a, P> Writer<'a> for EntriesEncoder<P> where P: Writer<'a> + Default {
        #[inline]
        fn get_buf_mut(&mut self) -> &mut WriteBuf<'a> {
            if let Some(parent) = self.parent.as_mut() {
                parent.get_buf_mut()
            } else {
                panic!("parent was None")
            }
        }
    }

    impl<'a, P> Encoder<'a> for EntriesEncoder<P> where P: Encoder<'a> + Default {
        #[inline]
        fn get_limit(&self) -> usize {
            self.parent.as_ref().expect("parent missing").get_limit()
        }

        #[inline]
        fn set_limit(&mut self, limit: usize) {
            self.parent.as_mut().expect("parent missing").set_limit(limit);
        }

        /// Set all optional fields to their 'null' values.
        #[inline]
        fn nullify_optional_fields(&mut self) -> &mut Self {
            self.order_id_opt(None);
            self.expires_at_opt(None);
            self.spend_cap_cents_opt(None);
            self.entered_stake_cents_opt(None);
            self.client_order_group_opt(None);
            self
        }
    }

    impl<'a, P> EntriesEncoder<P> where P: Encoder<'a> + Default {
        #[inline]
        pub fn wrap(
            mut self,
            mut parent: P,
            count: u16,
        ) -> Self {
            let initial_limit = parent.get_limit();
            parent.set_limit(initial_limit + 4);
            parent.get_buf_mut().put_u16_at(initial_limit, Self::block_length());
            parent.get_buf_mut().put_u16_at(initial_limit + 2, count);
            self.parent = Some(parent);
            self.count = count;
            self.index = usize::MAX;
            self.offset = usize::MAX;
            self.initial_limit = initial_limit;
            self
        }

        #[inline]
        pub const fn block_length() -> u16 {
            89
        }

        #[inline]
        pub fn parent(&mut self) -> SbeResult<P> {
            self.parent.take().ok_or(SbeErr::ParentNotSet)
        }

        /// will return Some(current index) when successful otherwise None
        #[inline]
        pub fn advance(&mut self) -> SbeResult<Option<usize>> {
            let index = self.index.wrapping_add(1);
            if index >= self.count as usize {
                return Ok(None);
            }
            if let Some(parent) = self.parent.as_mut() {
                self.offset = parent.get_limit();
                parent.set_limit(self.offset + Self::block_length() as usize);
                self.index = index;
                Ok(Some(index))
            } else {
                Err(SbeErr::ParentNotSet)
            }
        }

        /// REQUIRED enum
        #[inline]
        pub fn entry_type(&mut self, value: batch_entry_type::BatchEntryType) -> &mut Self {
            let offset = self.offset;
            self.get_buf_mut().put_u8_at(offset, value as u8);
            self
        }

        #[inline]
        pub fn client_order_id_at(&mut self, index: usize, value: u8) -> &mut Self {
            let offset = self.offset + 1;
            let buf = self.get_buf_mut();
            buf.put_u8_at(offset + index, value);
            self
        }

        /// primitive array field 'clientOrderId'
        /// - min value: 0
        /// - max value: 254
        /// - null value: 0xff_u8
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 1
        /// - encodedLength: 32
        /// - version: 0
        #[inline]
        pub fn client_order_id(&mut self, value: &[u8]) -> &mut Self {
            debug_assert_eq!(32, value.len());
            let offset = self.offset + 1;
            let buf = self.get_buf_mut();
            buf.put_slice_at(offset, value);
            self
        }

        /// primitive array field 'clientOrderId' from an Iterator
        /// - min value: 0
        /// - max value: 254
        /// - null value: 0xff_u8
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 1
        /// - encodedLength: 32
        /// - version: 0
        #[inline]
        pub fn client_order_id_from_iter(&mut self, iter: impl Iterator<Item = u8>) -> &mut Self {
            let offset = self.offset + 1;
            let buf = self.get_buf_mut();
            for (i, v) in iter.enumerate() {
                buf.put_u8_at(offset + i, v);
            }
            self
        }

        /// primitive array field 'clientOrderId' with zero padding
        /// - min value: 0
        /// - max value: 254
        /// - null value: 0xff_u8
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 1
        /// - encodedLength: 32
        /// - version: 0
        #[inline]
        pub fn client_order_id_zero_padded(&mut self, value: &[u8]) -> &mut Self {
            let iter = value.iter().copied().chain(core::iter::repeat(0_u8)).take(32);
            self.client_order_id_from_iter(iter);
            self
        }

        /// primitive field 'instrumentId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 33
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn instrument_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 33;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// primitive field 'orderId'
        /// - description: CANCEL by exchange order ID
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 41
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn order_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 41;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// optional primitive field 'orderId'
        /// - description: CANCEL by exchange order ID
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 41
        /// - encodedLength: 8
        /// - version: 0
        /// Set to `None` to encode the field null value.
        #[inline]
        pub fn order_id_opt(&mut self, value: Option<u64>) -> &mut Self {
            match value {
                Some(value) => self.order_id(value),
                None => self.order_id(0xffffffffffffffff_u64),
            };
            self
        }

        /// REQUIRED enum
        #[inline]
        pub fn side(&mut self, value: side::Side) -> &mut Self {
            let offset = self.offset + 49;
            self.get_buf_mut().put_u8_at(offset, value as u8);
            self
        }

        /// REQUIRED enum
        #[inline]
        pub fn entered_contract(&mut self, value: contract::Contract) -> &mut Self {
            let offset = self.offset + 50;
            self.get_buf_mut().put_u8_at(offset, value as u8);
            self
        }

        /// primitive field 'tick'
        /// - min value: 0
        /// - max value: 65534
        /// - null value: 0xffff_u16
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 51
        /// - encodedLength: 2
        /// - version: 0
        #[inline]
        pub fn tick(&mut self, value: u16) -> &mut Self {
            let offset = self.offset + 51;
            self.get_buf_mut().put_u16_at(offset, value);
            self
        }

        /// primitive field 'quantity'
        /// - min value: 0
        /// - max value: 4294967294
        /// - null value: 0xffffffff_u32
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 53
        /// - encodedLength: 4
        /// - version: 0
        #[inline]
        pub fn quantity(&mut self, value: u32) -> &mut Self {
            let offset = self.offset + 53;
            self.get_buf_mut().put_u32_at(offset, value);
            self
        }

        /// REQUIRED enum
        #[inline]
        pub fn time_in_force(&mut self, value: time_in_force::TimeInForce) -> &mut Self {
            let offset = self.offset + 57;
            self.get_buf_mut().put_u8_at(offset, value as u8);
            self
        }

        /// primitive field 'expiresAt'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 58
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn expires_at(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 58;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// optional primitive field 'expiresAt'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 58
        /// - encodedLength: 8
        /// - version: 0
        /// Set to `None` to encode the field null value.
        #[inline]
        pub fn expires_at_opt(&mut self, value: Option<u64>) -> &mut Self {
            match value {
                Some(value) => self.expires_at(value),
                None => self.expires_at(0xffffffffffffffff_u64),
            };
            self
        }

        /// REQUIRED enum
        #[inline]
        pub fn at_event_start(&mut self, value: at_event_start::AtEventStart) -> &mut Self {
            let offset = self.offset + 66;
            self.get_buf_mut().put_u8_at(offset, value as u8);
            self
        }

        #[inline]
        pub fn flags(&mut self, value: order_flags::OrderFlags) {
            let offset = self.offset + 67;
            self.get_buf_mut().put_u8_at(offset, value.0)
        }

        /// primitive field 'protectionTicks'
        /// - min value: 0
        /// - max value: 254
        /// - null value: 0xff_u8
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 68
        /// - encodedLength: 1
        /// - version: 0
        #[inline]
        pub fn protection_ticks(&mut self, value: u8) -> &mut Self {
            let offset = self.offset + 68;
            self.get_buf_mut().put_u8_at(offset, value);
            self
        }

        /// primitive field 'spendCapCents'
        /// - min value: -9223372036854775807
        /// - max value: 9223372036854775807
        /// - null value: -9223372036854775808_i64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 69
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn spend_cap_cents(&mut self, value: i64) -> &mut Self {
            let offset = self.offset + 69;
            self.get_buf_mut().put_i64_at(offset, value);
            self
        }

        /// optional primitive field 'spendCapCents'
        /// - min value: -9223372036854775807
        /// - max value: 9223372036854775807
        /// - null value: -9223372036854775808_i64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 69
        /// - encodedLength: 8
        /// - version: 0
        /// Set to `None` to encode the field null value.
        #[inline]
        pub fn spend_cap_cents_opt(&mut self, value: Option<i64>) -> &mut Self {
            match value {
                Some(value) => self.spend_cap_cents(value),
                None => self.spend_cap_cents(-9223372036854775808_i64),
            };
            self
        }

        /// primitive field 'enteredStakeCents'
        /// - min value: -9223372036854775807
        /// - max value: 9223372036854775807
        /// - null value: -9223372036854775808_i64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 77
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn entered_stake_cents(&mut self, value: i64) -> &mut Self {
            let offset = self.offset + 77;
            self.get_buf_mut().put_i64_at(offset, value);
            self
        }

        /// optional primitive field 'enteredStakeCents'
        /// - min value: -9223372036854775807
        /// - max value: 9223372036854775807
        /// - null value: -9223372036854775808_i64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 77
        /// - encodedLength: 8
        /// - version: 0
        /// Set to `None` to encode the field null value.
        #[inline]
        pub fn entered_stake_cents_opt(&mut self, value: Option<i64>) -> &mut Self {
            match value {
                Some(value) => self.entered_stake_cents(value),
                None => self.entered_stake_cents(-9223372036854775808_i64),
            };
            self
        }

        /// primitive field 'clientOrderGroup'
        /// - min value: 0
        /// - max value: 4294967294
        /// - null value: 0xffffffff_u32
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 85
        /// - encodedLength: 4
        /// - version: 0
        #[inline]
        pub fn client_order_group(&mut self, value: u32) -> &mut Self {
            let offset = self.offset + 85;
            self.get_buf_mut().put_u32_at(offset, value);
            self
        }

        /// optional primitive field 'clientOrderGroup'
        /// - min value: 0
        /// - max value: 4294967294
        /// - null value: 0xffffffff_u32
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 85
        /// - encodedLength: 4
        /// - version: 0
        /// Set to `None` to encode the field null value.
        #[inline]
        pub fn client_order_group_opt(&mut self, value: Option<u32>) -> &mut Self {
            match value {
                Some(value) => self.client_order_group(value),
                None => self.client_order_group(0xffffffff_u32),
            };
            self
        }

    }

} // end encoder

pub mod decoder {
    use super::*;
    use message_header_codec::*;

    #[derive(Clone, Copy, Debug, Default)]
    pub struct BatchOrdersDecoder<'a> {
        buf: ReadBuf<'a>,
        initial_offset: usize,
        offset: usize,
        limit: usize,
        pub acting_block_length: u16,
        pub acting_version: u16,
    }

    impl ActingVersion for BatchOrdersDecoder<'_> {
        #[inline]
        fn acting_version(&self) -> u16 {
            self.acting_version
        }
    }

    impl<'a> Reader<'a> for BatchOrdersDecoder<'a> {
        #[inline]
        fn get_buf(&self) -> &ReadBuf<'a> {
            &self.buf
        }
    }

    impl<'a> Decoder<'a> for BatchOrdersDecoder<'a> {
        #[inline]
        fn get_limit(&self) -> usize {
            self.limit
        }

        #[inline]
        fn set_limit(&mut self, limit: usize) {
            self.limit = limit;
        }
    }

    impl<'a> BatchOrdersDecoder<'a> {
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
        pub fn record_decoder(self) -> record_header_codec::RecordHeaderDecoder<Self> {
            let offset = self.offset;
            record_header_codec::RecordHeaderDecoder::default().wrap(self, offset)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn session_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 18)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn account_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 26)
        }

        /// GROUP DECODER (id=10)
        #[inline]
        pub fn entries_decoder(self) -> EntriesDecoder<Self> {
            EntriesDecoder::default().wrap(self)
        }

    }

    #[derive(Debug, Default)]
    pub struct EntriesDecoder<P> {
        parent: Option<P>,
        block_length: u16,
        count: u16,
        index: usize,
        offset: usize,
    }

    impl<'a, P> ActingVersion for EntriesDecoder<P> where P: Reader<'a> + ActingVersion + Default {
        #[inline]
        fn acting_version(&self) -> u16 {
            self.parent.as_ref().unwrap().acting_version()
        }
    }

    impl<'a, P> Reader<'a> for EntriesDecoder<P> where P: Reader<'a> + Default {
        #[inline]
        fn get_buf(&self) -> &ReadBuf<'a> {
            self.parent.as_ref().expect("parent missing").get_buf()
        }
    }

    impl<'a, P> Decoder<'a> for EntriesDecoder<P> where P: Decoder<'a> + ActingVersion + Default {
        #[inline]
        fn get_limit(&self) -> usize {
            self.parent.as_ref().expect("parent missing").get_limit()
        }

        #[inline]
        fn set_limit(&mut self, limit: usize) {
            self.parent.as_mut().expect("parent missing").set_limit(limit);
        }
    }

    impl<'a, P> EntriesDecoder<P> where P: Decoder<'a> + ActingVersion + Default {
        pub fn wrap(
            mut self,
            mut parent: P,
        ) -> Self {
            let initial_offset = parent.get_limit();
            let block_length = parent.get_buf().get_u16_at(initial_offset);
            let count = parent.get_buf().get_u16_at(initial_offset + 2);
            parent.set_limit(initial_offset + 4);
            self.parent = Some(parent);
            self.block_length = block_length;
            self.count = count;
            self.index = usize::MAX;
            self.offset = 0;
            self
        }

        /// group token - Token{signal=BEGIN_GROUP, name='entries', referencedName='null', description='null', packageName='null', id=10, version=0, deprecated=0, encodedLength=89, offset=34, componentTokenCount=75, encoding=Encoding{presence=REQUIRED, primitiveType=null, byteOrder=LITTLE_ENDIAN, minValue=null, maxValue=null, nullValue=null, constValue=null, characterEncoding='null', epoch='null', timeUnit=null, semanticType='null'}}
        #[inline]
        pub fn parent(&mut self) -> SbeResult<P> {
            self.parent.take().ok_or(SbeErr::ParentNotSet)
        }

        #[inline]
        pub fn acting_version(&mut self) -> u16 {
            self.parent.as_ref().unwrap().acting_version()
        }

        #[inline]
        pub fn count(&self) -> u16 {
            self.count
        }

        /// will return Some(current index) when successful otherwise None
        pub fn advance(&mut self) -> SbeResult<Option<usize>> {
            let index = self.index.wrapping_add(1);
            if index >= self.count as usize {
                 return Ok(None);
            }
            if let Some(parent) = self.parent.as_mut() {
                self.offset = parent.get_limit();
                parent.set_limit(self.offset + self.block_length as usize);
                self.index = index;
                Ok(Some(index))
            } else {
                Err(SbeErr::ParentNotSet)
            }
        }

        /// REQUIRED enum
        #[inline]
        pub fn entry_type(&self) -> batch_entry_type::BatchEntryType {
            self.get_buf().get_u8_at(self.offset).into()
        }

        #[inline]
        pub fn client_order_id(&self) -> [u8; 32] {
            let buf = self.get_buf();
            ReadBuf::get_bytes_at(buf.data, self.offset + 1)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn instrument_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 33)
        }

        /// primitive field - 'OPTIONAL' { null_value: '0xffffffffffffffff_u64' }
        /// - description: CANCEL by exchange order ID
        #[inline]
        pub fn order_id(&self) -> Option<u64> {
            let value = self.get_buf().get_u64_at(self.offset + 41);
            if value == 0xffffffffffffffff_u64 {
                None
            } else {
                Some(value)
            }
        }

        /// REQUIRED enum
        #[inline]
        pub fn side(&self) -> side::Side {
            self.get_buf().get_u8_at(self.offset + 49).into()
        }

        /// REQUIRED enum
        #[inline]
        pub fn entered_contract(&self) -> contract::Contract {
            self.get_buf().get_u8_at(self.offset + 50).into()
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn tick(&self) -> u16 {
            self.get_buf().get_u16_at(self.offset + 51)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn quantity(&self) -> u32 {
            self.get_buf().get_u32_at(self.offset + 53)
        }

        /// REQUIRED enum
        #[inline]
        pub fn time_in_force(&self) -> time_in_force::TimeInForce {
            self.get_buf().get_u8_at(self.offset + 57).into()
        }

        /// primitive field - 'OPTIONAL' { null_value: '0xffffffffffffffff_u64' }
        #[inline]
        pub fn expires_at(&self) -> Option<u64> {
            let value = self.get_buf().get_u64_at(self.offset + 58);
            if value == 0xffffffffffffffff_u64 {
                None
            } else {
                Some(value)
            }
        }

        /// REQUIRED enum
        #[inline]
        pub fn at_event_start(&self) -> at_event_start::AtEventStart {
            self.get_buf().get_u8_at(self.offset + 66).into()
        }

        /// BIT SET DECODER
        #[inline]
        pub fn flags(&self) -> order_flags::OrderFlags {
            order_flags::OrderFlags::new(self.get_buf().get_u8_at(self.offset + 67))
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn protection_ticks(&self) -> u8 {
            self.get_buf().get_u8_at(self.offset + 68)
        }

        /// primitive field - 'OPTIONAL' { null_value: '-9223372036854775808_i64' }
        #[inline]
        pub fn spend_cap_cents(&self) -> Option<i64> {
            let value = self.get_buf().get_i64_at(self.offset + 69);
            if value == -9223372036854775808_i64 {
                None
            } else {
                Some(value)
            }
        }

        /// primitive field - 'OPTIONAL' { null_value: '-9223372036854775808_i64' }
        #[inline]
        pub fn entered_stake_cents(&self) -> Option<i64> {
            let value = self.get_buf().get_i64_at(self.offset + 77);
            if value == -9223372036854775808_i64 {
                None
            } else {
                Some(value)
            }
        }

        /// primitive field - 'OPTIONAL' { null_value: '0xffffffff_u32' }
        #[inline]
        pub fn client_order_group(&self) -> Option<u32> {
            let value = self.get_buf().get_u32_at(self.offset + 85);
            if value == 0xffffffff_u32 {
                None
            } else {
                Some(value)
            }
        }

    }

} // end decoder

