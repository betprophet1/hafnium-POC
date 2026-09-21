use crate::*;

pub use decoder::BookLevelChangedDecoder;
pub use encoder::BookLevelChangedEncoder;

pub use crate::SBE_SCHEMA_ID;
pub use crate::SBE_SCHEMA_VERSION;
pub use crate::SBE_SEMANTIC_VERSION;

pub const SBE_BLOCK_LENGTH: u16 = 41;
pub const SBE_TEMPLATE_ID: u16 = 109;

pub mod encoder {
    use super::*;
    use message_header_codec::*;

    #[derive(Debug, Default)]
    pub struct BookLevelChangedEncoder<'a> {
        buf: WriteBuf<'a>,
        initial_offset: usize,
        offset: usize,
        limit: usize,
    }

    impl<'a> Writer<'a> for BookLevelChangedEncoder<'a> {
        #[inline]
        fn get_buf_mut(&mut self) -> &mut WriteBuf<'a> {
            &mut self.buf
        }
    }

    impl<'a> Encoder<'a> for BookLevelChangedEncoder<'a> {
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

    impl<'a> BookLevelChangedEncoder<'a> {
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

        /// primitive field 'instrumentId'
        /// - min value: 0
        /// - max value: -2
        /// - null value: 0xffffffffffffffff_u64
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 22
        /// - encodedLength: 8
        /// - version: 0
        #[inline]
        pub fn instrument_id(&mut self, value: u64) -> &mut Self {
            let offset = self.offset + 22;
            self.get_buf_mut().put_u64_at(offset, value);
            self
        }

        /// REQUIRED enum
        #[inline]
        pub fn side(&mut self, value: side::Side) -> &mut Self {
            let offset = self.offset + 30;
            self.get_buf_mut().put_u8_at(offset, value as u8);
            self
        }

        /// primitive field 'tick'
        /// - min value: 0
        /// - max value: 65534
        /// - null value: 0xffff_u16
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 31
        /// - encodedLength: 2
        /// - version: 0
        #[inline]
        pub fn tick(&mut self, value: u16) -> &mut Self {
            let offset = self.offset + 31;
            self.get_buf_mut().put_u16_at(offset, value);
            self
        }

        /// primitive field 'quantity'
        /// - min value: 0
        /// - max value: 4294967294
        /// - null value: 0xffffffff_u32
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 33
        /// - encodedLength: 4
        /// - version: 0
        #[inline]
        pub fn quantity(&mut self, value: u32) -> &mut Self {
            let offset = self.offset + 33;
            self.get_buf_mut().put_u32_at(offset, value);
            self
        }

        /// primitive field 'orderCount'
        /// - min value: 0
        /// - max value: 4294967294
        /// - null value: 0xffffffff_u32
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 37
        /// - encodedLength: 4
        /// - version: 0
        #[inline]
        pub fn order_count(&mut self, value: u32) -> &mut Self {
            let offset = self.offset + 37;
            self.get_buf_mut().put_u32_at(offset, value);
            self
        }
    }
} // end encoder

pub mod decoder {
    use super::*;
    use message_header_codec::*;

    #[derive(Clone, Copy, Debug, Default)]
    pub struct BookLevelChangedDecoder<'a> {
        buf: ReadBuf<'a>,
        initial_offset: usize,
        offset: usize,
        limit: usize,
        pub acting_block_length: u16,
        pub acting_version: u16,
    }

    impl ActingVersion for BookLevelChangedDecoder<'_> {
        #[inline]
        fn acting_version(&self) -> u16 {
            self.acting_version
        }
    }

    impl<'a> Reader<'a> for BookLevelChangedDecoder<'a> {
        #[inline]
        fn get_buf(&self) -> &ReadBuf<'a> {
            &self.buf
        }
    }

    impl<'a> Decoder<'a> for BookLevelChangedDecoder<'a> {
        #[inline]
        fn get_limit(&self) -> usize {
            self.limit
        }

        #[inline]
        fn set_limit(&mut self, limit: usize) {
            self.limit = limit;
        }
    }

    impl<'a> BookLevelChangedDecoder<'a> {
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
        pub fn result_decoder(self) -> result_header_codec::ResultHeaderDecoder<Self> {
            let offset = self.offset;
            result_header_codec::ResultHeaderDecoder::default().wrap(self, offset)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn instrument_id(&self) -> u64 {
            self.get_buf().get_u64_at(self.offset + 22)
        }

        /// REQUIRED enum
        #[inline]
        pub fn side(&self) -> side::Side {
            self.get_buf().get_u8_at(self.offset + 30).into()
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn tick(&self) -> u16 {
            self.get_buf().get_u16_at(self.offset + 31)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn quantity(&self) -> u32 {
            self.get_buf().get_u32_at(self.offset + 33)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn order_count(&self) -> u32 {
            self.get_buf().get_u32_at(self.offset + 37)
        }
    }
} // end decoder
