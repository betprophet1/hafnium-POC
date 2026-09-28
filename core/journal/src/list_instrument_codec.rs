use crate::*;

pub use decoder::ListInstrumentDecoder;
pub use encoder::ListInstrumentEncoder;

pub use crate::SBE_SCHEMA_ID;
pub use crate::SBE_SCHEMA_VERSION;
pub use crate::SBE_SEMANTIC_VERSION;

pub const SBE_BLOCK_LENGTH: u16 = 72;
pub const SBE_TEMPLATE_ID: u16 = 84;

pub mod encoder {
    use super::*;
    use message_header_codec::*;

    #[derive(Debug, Default)]
    pub struct ListInstrumentEncoder<'a> {
        buf: WriteBuf<'a>,
        initial_offset: usize,
        offset: usize,
        limit: usize,
    }

    impl<'a> Writer<'a> for ListInstrumentEncoder<'a> {
        #[inline]
        fn get_buf_mut(&mut self) -> &mut WriteBuf<'a> {
            &mut self.buf
        }
    }

    impl<'a> Encoder<'a> for ListInstrumentEncoder<'a> {
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

    impl<'a> ListInstrumentEncoder<'a> {
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

        #[inline]
        pub fn command_id_at(&mut self, index: usize, value: u8) -> &mut Self {
            let offset = self.offset + 18;
            let buf = self.get_buf_mut();
            buf.put_u8_at(offset + index, value);
            self
        }

        /// primitive array field 'commandId'
        /// - min value: 0
        /// - max value: 254
        /// - null value: 0xff_u8
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 18
        /// - encodedLength: 16
        /// - version: 0
        #[inline]
        pub fn command_id(&mut self, value: &[u8]) -> &mut Self {
            debug_assert_eq!(16, value.len());
            let offset = self.offset + 18;
            let buf = self.get_buf_mut();
            buf.put_slice_at(offset, value);
            self
        }

        /// primitive array field 'commandId' from an Iterator
        /// - min value: 0
        /// - max value: 254
        /// - null value: 0xff_u8
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 18
        /// - encodedLength: 16
        /// - version: 0
        #[inline]
        pub fn command_id_from_iter(&mut self, iter: impl Iterator<Item = u8>) -> &mut Self {
            let offset = self.offset + 18;
            let buf = self.get_buf_mut();
            for (i, v) in iter.enumerate() {
                buf.put_u8_at(offset + i, v);
            }
            self
        }

        /// primitive array field 'commandId' with zero padding
        /// - min value: 0
        /// - max value: 254
        /// - null value: 0xff_u8
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 18
        /// - encodedLength: 16
        /// - version: 0
        #[inline]
        pub fn command_id_zero_padded(&mut self, value: &[u8]) -> &mut Self {
            let iter = value.iter().copied().chain(core::iter::repeat(0_u8)).take(16);
            self.command_id_from_iter(iter);
            self
        }

        /// primitive field 'instrumentId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 34
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn instrument_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 34;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// primitive field 'eventId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 42
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn event_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 42;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// REQUIRED enum
        #[inline]
        pub fn instrument_class(&mut self, value: instrument_class::InstrumentClass) -> &mut Self {
            let offset = self.offset + 50;
            self.get_buf_mut().put_u8_at(offset, value as u8);
            self
        }

        /// primitive field 'tickTableId'
        /// - min value: 0
        /// - max value: 4294967294
        /// - null value: 0xffffffff_u32
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 51
        /// - encodedLength: 4
        /// - version: 0
        #[inline]
        pub fn tick_table_id(&mut self, value: u32) -> &mut Self {
            let offset = self.offset + 51;
            self.get_buf_mut().put_u32_at(offset, value);
            self
        }

        /// primitive field 'lotStep'
        /// - min value: 0
        /// - max value: 4294967294
        /// - null value: 0xffffffff_u32
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 55
        /// - encodedLength: 4
        /// - version: 0
        #[inline]
        pub fn lot_step(&mut self, value: u32) -> &mut Self {
            let offset = self.offset + 55;
            self.get_buf_mut().put_u32_at(offset, value);
            self
        }

        /// REQUIRED enum
        #[inline]
        pub fn allocation_policy(&mut self, value: allocation_policy::AllocationPolicy) -> &mut Self {
            let offset = self.offset + 59;
            self.get_buf_mut().put_u8_at(offset, value as u8);
            self
        }

        /// primitive field 'stateMask'
        /// - description: States in which a YES of this book pays $1.00
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 60
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn state_mask(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 60;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// primitive field 'positionLimit'
        /// - description: Largest absolute position per account, in contracts; 0 for none
        /// - min value: 0
        /// - max value: 4294967294
        /// - null value: 0xffffffff_u32
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 68
        /// - encodedLength: 4
        /// - version: 0
        #[inline]
        pub fn position_limit(&mut self, value: u32) -> &mut Self {
            let offset = self.offset + 68;
            self.get_buf_mut().put_u32_at(offset, value);
            self
        }

        /// GROUP ENCODER (id=10)
        #[inline]
        pub fn legs_encoder(self, count: u16, legs_encoder: LegsEncoder<Self>) -> LegsEncoder<Self> {
            legs_encoder.wrap(self, count)
        }
    }

    #[derive(Debug, Default)]
    pub struct LegsEncoder<P> {
        parent: Option<P>,
        count: u16,
        index: usize,
        offset: usize,
        initial_limit: usize,
    }

    impl<'a, P> Writer<'a> for LegsEncoder<P>
    where
        P: Writer<'a> + Default,
    {
        #[inline]
        fn get_buf_mut(&mut self) -> &mut WriteBuf<'a> {
            if let Some(parent) = self.parent.as_mut() {
                parent.get_buf_mut()
            } else {
                panic!("parent was None")
            }
        }
    }

    impl<'a, P> Encoder<'a> for LegsEncoder<P>
    where
        P: Encoder<'a> + Default,
    {
        #[inline]
        fn get_limit(&self) -> usize {
            self.parent.as_ref().expect("parent missing").get_limit()
        }

        #[inline]
        fn set_limit(&mut self, limit: usize) {
            self.parent.as_mut().expect("parent missing").set_limit(limit);
        }
    }

    impl<'a, P> LegsEncoder<P>
    where
        P: Encoder<'a> + Default,
    {
        #[inline]
        pub fn wrap(mut self, mut parent: P, count: u16) -> Self {
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
            17
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

        /// primitive field 'legEventId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 0
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn leg_event_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// primitive field 'legInstrumentId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 8
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn leg_instrument_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 8;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// REQUIRED enum
        #[inline]
        pub fn leg_outcome(&mut self, value: outcome_value::OutcomeValue) -> &mut Self {
            let offset = self.offset + 16;
            self.get_buf_mut().put_u8_at(offset, value as u8);
            self
        }
    }
} // end encoder

pub mod decoder {
    use super::*;
    use message_header_codec::*;

    #[derive(Clone, Copy, Debug, Default)]
    pub struct ListInstrumentDecoder<'a> {
        buf: ReadBuf<'a>,
        initial_offset: usize,
        offset: usize,
        limit: usize,
        pub acting_block_length: u16,
        pub acting_version: u16,
    }

    impl ActingVersion for ListInstrumentDecoder<'_> {
        #[inline]
        fn acting_version(&self) -> u16 {
            self.acting_version
        }
    }

    impl<'a> Reader<'a> for ListInstrumentDecoder<'a> {
        #[inline]
        fn get_buf(&self) -> &ReadBuf<'a> {
            &self.buf
        }
    }

    impl<'a> Decoder<'a> for ListInstrumentDecoder<'a> {
        #[inline]
        fn get_limit(&self) -> usize {
            self.limit
        }

        #[inline]
        fn set_limit(&mut self, limit: usize) {
            self.limit = limit;
        }
    }

    impl<'a> ListInstrumentDecoder<'a> {
        pub fn wrap(mut self, buf: ReadBuf<'a>, offset: usize, acting_block_length: u16, acting_version: u16) -> Self {
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

        #[inline]
        pub fn command_id(&self) -> [u8; 16] {
            let buf = self.get_buf();
            ReadBuf::get_bytes_at(buf.data, self.offset + 18)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn instrument_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 34)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn event_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 42)
        }

        /// REQUIRED enum
        #[inline]
        pub fn instrument_class(&self) -> instrument_class::InstrumentClass {
            self.get_buf().get_u8_at(self.offset + 50).into()
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn tick_table_id(&self) -> u32 {
            self.get_buf().get_u32_at(self.offset + 51)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn lot_step(&self) -> u32 {
            self.get_buf().get_u32_at(self.offset + 55)
        }

        /// REQUIRED enum
        #[inline]
        pub fn allocation_policy(&self) -> allocation_policy::AllocationPolicy {
            self.get_buf().get_u8_at(self.offset + 59).into()
        }

        /// primitive field - 'REQUIRED'
        /// - description: States in which a YES of this book pays $1.00
        #[inline]
        pub fn state_mask(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 60)
        }

        /// primitive field - 'REQUIRED'
        /// - description: Largest absolute position per account, in contracts; 0 for none
        #[inline]
        pub fn position_limit(&self) -> u32 {
            self.get_buf().get_u32_at(self.offset + 68)
        }

        /// GROUP DECODER (id=10)
        #[inline]
        pub fn legs_decoder(self) -> LegsDecoder<Self> {
            LegsDecoder::default().wrap(self)
        }
    }

    #[derive(Debug, Default)]
    pub struct LegsDecoder<P> {
        parent: Option<P>,
        block_length: u16,
        count: u16,
        index: usize,
        offset: usize,
    }

    impl<'a, P> ActingVersion for LegsDecoder<P>
    where
        P: Reader<'a> + ActingVersion + Default,
    {
        #[inline]
        fn acting_version(&self) -> u16 {
            self.parent.as_ref().unwrap().acting_version()
        }
    }

    impl<'a, P> Reader<'a> for LegsDecoder<P>
    where
        P: Reader<'a> + Default,
    {
        #[inline]
        fn get_buf(&self) -> &ReadBuf<'a> {
            self.parent.as_ref().expect("parent missing").get_buf()
        }
    }

    impl<'a, P> Decoder<'a> for LegsDecoder<P>
    where
        P: Decoder<'a> + ActingVersion + Default,
    {
        #[inline]
        fn get_limit(&self) -> usize {
            self.parent.as_ref().expect("parent missing").get_limit()
        }

        #[inline]
        fn set_limit(&mut self, limit: usize) {
            self.parent.as_mut().expect("parent missing").set_limit(limit);
        }
    }

    impl<'a, P> LegsDecoder<P>
    where
        P: Decoder<'a> + ActingVersion + Default,
    {
        pub fn wrap(mut self, mut parent: P) -> Self {
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

        /// group token - Token{signal=BEGIN_GROUP, name='legs', referencedName='null', description='null', packageName='null', id=10, version=0, deprecated=0, encodedLength=17, offset=72, componentTokenCount=18, encoding=Encoding{presence=REQUIRED, primitiveType=null, byteOrder=LITTLE_ENDIAN, minValue=null, maxValue=null, nullValue=null, constValue=null, characterEncoding='null', epoch='null', timeUnit=null, semanticType='null'}}
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

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn leg_event_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn leg_instrument_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 8)
        }

        /// REQUIRED enum
        #[inline]
        pub fn leg_outcome(&self) -> outcome_value::OutcomeValue {
            self.get_buf().get_u8_at(self.offset + 16).into()
        }
    }
} // end decoder
