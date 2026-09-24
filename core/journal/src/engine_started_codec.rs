use crate::*;

pub use decoder::EngineStartedDecoder;
pub use encoder::EngineStartedEncoder;

pub use crate::SBE_SCHEMA_ID;
pub use crate::SBE_SCHEMA_VERSION;
pub use crate::SBE_SEMANTIC_VERSION;

pub const SBE_BLOCK_LENGTH: u16 = 54;
pub const SBE_TEMPLATE_ID: u16 = 51;

pub mod encoder {
    use super::*;
    use message_header_codec::*;

    #[derive(Debug, Default)]
    pub struct EngineStartedEncoder<'a> {
        buf: WriteBuf<'a>,
        initial_offset: usize,
        offset: usize,
        limit: usize,
    }

    impl<'a> Writer<'a> for EngineStartedEncoder<'a> {
        #[inline]
        fn get_buf_mut(&mut self) -> &mut WriteBuf<'a> {
            &mut self.buf
        }
    }

    impl<'a> Encoder<'a> for EngineStartedEncoder<'a> {
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

    impl<'a> EngineStartedEncoder<'a> {
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

        /// primitive field 'nodeId'
        /// - min value: 0
        /// - max value: 65534
        /// - null value: 0xffff_u16
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 18
        /// - encodedLength: 2
        /// - version: 0
        #[inline]
        pub fn node_id(&mut self, value: u16) -> &mut Self {
            let offset = self.offset + 18;
            self.get_buf_mut().put_u16_at(offset, value);
            self
        }

        /// primitive field 'protocolVersion'
        /// - min value: 0
        /// - max value: 65534
        /// - null value: 0xffff_u16
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 20
        /// - encodedLength: 2
        /// - version: 0
        #[inline]
        pub fn protocol_version(&mut self, value: u16) -> &mut Self {
            let offset = self.offset + 20;
            self.get_buf_mut().put_u16_at(offset, value);
            self
        }

        #[inline]
        pub fn binary_hash_at(&mut self, index: usize, value: u8) -> &mut Self {
            let offset = self.offset + 22;
            let buf = self.get_buf_mut();
            buf.put_u8_at(offset + index, value);
            self
        }

        /// primitive array field 'binaryHash'
        /// - min value: 0
        /// - max value: 254
        /// - null value: 0xff_u8
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 22
        /// - encodedLength: 32
        /// - version: 0
        #[inline]
        pub fn binary_hash(&mut self, value: &[u8]) -> &mut Self {
            debug_assert_eq!(32, value.len());
            let offset = self.offset + 22;
            let buf = self.get_buf_mut();
            buf.put_slice_at(offset, value);
            self
        }

        /// primitive array field 'binaryHash' from an Iterator
        /// - min value: 0
        /// - max value: 254
        /// - null value: 0xff_u8
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 22
        /// - encodedLength: 32
        /// - version: 0
        #[inline]
        pub fn binary_hash_from_iter(&mut self, iter: impl Iterator<Item = u8>) -> &mut Self {
            let offset = self.offset + 22;
            let buf = self.get_buf_mut();
            for (i, v) in iter.enumerate() {
                buf.put_u8_at(offset + i, v);
            }
            self
        }

        /// primitive array field 'binaryHash' with zero padding
        /// - min value: 0
        /// - max value: 254
        /// - null value: 0xff_u8
        /// - characterEncoding: null
        /// - semanticType: null
        /// - encodedOffset: 22
        /// - encodedLength: 32
        /// - version: 0
        #[inline]
        pub fn binary_hash_zero_padded(&mut self, value: &[u8]) -> &mut Self {
            let iter = value.iter().copied().chain(core::iter::repeat(0_u8)).take(32);
            self.binary_hash_from_iter(iter);
            self
        }

    }

} // end encoder

pub mod decoder {
    use super::*;
    use message_header_codec::*;

    #[derive(Clone, Copy, Debug, Default)]
    pub struct EngineStartedDecoder<'a> {
        buf: ReadBuf<'a>,
        initial_offset: usize,
        offset: usize,
        limit: usize,
        pub acting_block_length: u16,
        pub acting_version: u16,
    }

    impl ActingVersion for EngineStartedDecoder<'_> {
        #[inline]
        fn acting_version(&self) -> u16 {
            self.acting_version
        }
    }

    impl<'a> Reader<'a> for EngineStartedDecoder<'a> {
        #[inline]
        fn get_buf(&self) -> &ReadBuf<'a> {
            &self.buf
        }
    }

    impl<'a> Decoder<'a> for EngineStartedDecoder<'a> {
        #[inline]
        fn get_limit(&self) -> usize {
            self.limit
        }

        #[inline]
        fn set_limit(&mut self, limit: usize) {
            self.limit = limit;
        }
    }

    impl<'a> EngineStartedDecoder<'a> {
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
        pub fn node_id(&self) -> u16 {
            self.get_buf().get_u16_at(self.offset + 18)
        }

        /// primitive field - 'REQUIRED'
        #[inline]
        pub fn protocol_version(&self) -> u16 {
            self.get_buf().get_u16_at(self.offset + 20)
        }

        #[inline]
        pub fn binary_hash(&self) -> [u8; 32] {
            let buf = self.get_buf();
            ReadBuf::get_bytes_at(buf.data, self.offset + 22)
        }

    }

} // end decoder

