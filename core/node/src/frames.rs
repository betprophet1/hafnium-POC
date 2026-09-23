//! The one framing used by journal files, result logs and the TCP ingress: a little-endian `u32` byte length,
//! then that many bytes of SBE, message header first.

use std::io::{self, ErrorKind, Read, Write};

/// The longest frame accepted. Far above any message in the schema; refuses garbage lengths.
pub const MAX_FRAME: usize = 1 << 16;

pub fn write_frame(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len())
        .ok()
        .filter(|&n| n as usize <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "frame longer than MAX_FRAME"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(bytes)
}

/// A frame's template id, read without decoding it: every SBE message header carries it at this fixed offset.
/// What lets a reader recognise a `Snapshot` frame (template 53) among inputs without a full decode — it is not
/// one (interfaces/sbe/venue-journal.xml, Snapshot), and every reader that walks a journal frame by frame skips
/// it by this same check rather than trying to apply or pair it like an input.
pub fn template_of(frame: &[u8]) -> Option<u16> {
    frame.get(2..4).map(|b| u16::from_le_bytes(b.try_into().unwrap_or_default()))
}

/// Reads one frame into `buf` and returns its length, or `None` at a clean end of stream. A stream that ends
/// inside a frame is an `UnexpectedEof` error: a torn write.
pub fn read_frame(r: &mut impl Read, buf: &mut Vec<u8>) -> io::Result<Option<usize>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(ErrorKind::InvalidData, "frame longer than MAX_FRAME"));
    }
    buf.resize(n, 0);
    r.read_exact(&mut buf[..n])?;
    Ok(Some(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_a_torn_tail_is_an_error() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, b"abc").unwrap();
        write_frame(&mut bytes, b"").unwrap();
        let mut r = &bytes[..];
        let mut buf = Vec::new();
        assert_eq!(read_frame(&mut r, &mut buf).unwrap(), Some(3));
        assert_eq!(&buf, b"abc");
        assert_eq!(read_frame(&mut r, &mut buf).unwrap(), Some(0));
        assert_eq!(read_frame(&mut r, &mut buf).unwrap(), None);
        let torn = &bytes[..5];
        let mut r = torn;
        assert_eq!(read_frame(&mut r, &mut buf).unwrap_err().kind(), ErrorKind::UnexpectedEof);
    }
}
