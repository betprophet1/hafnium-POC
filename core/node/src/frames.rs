//! The one framing used by journal files, result logs and the TCP ingress: a little-endian `u32` byte length,
//! then that many bytes of SBE, message header first.

use std::io::{self, ErrorKind, Read, Write};

use venue_journal::snapshot_codec::SBE_TEMPLATE_ID as SNAPSHOT;

/// The longest *input* frame accepted. Far above any input message in the schema; refuses garbage lengths.
pub const MAX_FRAME: usize = 1 << 16;

/// The longest `Snapshot` frame accepted, which is a different question. An input message is bounded by its
/// schema — no arrangement of fields makes a `PlaceOrder` large — but a snapshot is the engine's whole logical
/// state, so its size grows with the venue: instruments, resting orders, positions. Holding it to the input
/// bound means a venue can snapshot itself only while it is small, and stops being able to at no stated moment
/// and with no warning — and since a journal with no snapshot in it can never be `cut` (§20, D-32), the disk it
/// was supposed to stop filling fills anyway. This is the buffer `snapshots::compute` already encodes into, so
/// the write bound and the encode bound are now the same number rather than differing by a factor of 256.
pub const MAX_SNAPSHOT_FRAME: usize = 1 << 24;

/// The bound a frame is held to, which depends on what it is: only a `Snapshot` gets the larger one.
fn limit(frame: &[u8]) -> usize {
    if template_of(frame) == Some(SNAPSHOT) {
        MAX_SNAPSHOT_FRAME
    } else {
        MAX_FRAME
    }
}

fn too_long(bytes: usize, limit: usize, template: Option<u16>) -> io::Error {
    let kind = match template {
        Some(t) => format!("template {t}"),
        None => "a frame too short to carry a template".to_string(),
    };
    // The sizes and the template are in the message because the bound that bites is not always the one the
    // reader expects, and "frame longer than MAX_FRAME" alone does not say which frame, or by how much.
    io::Error::new(
        ErrorKind::InvalidData,
        format!("frame longer than MAX_FRAME: {bytes} bytes for {kind}, limit {limit}"),
    )
}

pub fn write_frame(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    let limit = limit(bytes);
    let len = u32::try_from(bytes.len())
        .ok()
        .filter(|&n| n as usize <= limit)
        .ok_or_else(|| too_long(bytes.len(), limit, template_of(bytes)))?;
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
        // A length this large is either a `Snapshot` or garbage, and the length alone does not say which. The
        // template sits in the header's first four bytes, so read only those and decide before committing to an
        // allocation the length has not yet earned — a garbage length still costs four bytes, not sixteen
        // megabytes.
        if n > MAX_SNAPSHOT_FRAME {
            return Err(too_long(n, MAX_SNAPSHOT_FRAME, None));
        }
        buf.resize(4, 0);
        r.read_exact(&mut buf[..4])?;
        if template_of(&buf[..4]) != Some(SNAPSHOT) {
            return Err(too_long(n, MAX_FRAME, template_of(&buf[..4])));
        }
        buf.resize(n, 0);
        r.read_exact(&mut buf[4..n])?;
        return Ok(Some(n));
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

    /// A frame carrying `template` at the fixed header offset, `len` bytes long. Framing looks at nothing else,
    /// so the body does not have to decode for these.
    fn framed(template: u16, len: usize) -> Vec<u8> {
        let mut frame = vec![0u8; len];
        frame[2..4].copy_from_slice(&template.to_le_bytes());
        frame
    }

    /// The bug this bound was written for. A real venue's engine state passed 64 KiB somewhere between its
    /// 30,000th and 60,000th input — a few hours of demo traffic — and from that moment `snapshot` and
    /// `embed-snapshot` both failed with nothing but "frame longer than MAX_FRAME" to say why. Since `cut`
    /// needs an embedded snapshot, the journal could no longer be trimmed either: the capacity work stopped
    /// working precisely when there was enough data for it to matter.
    #[test]
    fn a_snapshot_larger_than_an_input_may_be_survives_the_round_trip() {
        let big = framed(SNAPSHOT, MAX_FRAME + 1);
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &big).expect("a snapshot past the input bound writes");

        let mut r = &bytes[..];
        let mut buf = Vec::new();
        assert_eq!(read_frame(&mut r, &mut buf).unwrap(), Some(big.len()), "it did not read back");
        assert_eq!(buf, big, "the bytes changed crossing the boundary");
    }

    /// And the input bound is still a bound: the larger one belongs to `Snapshot` alone, not to any frame that
    /// happens to be long. Were this to pass for every template, a garbage length would become an allocation.
    #[test]
    fn nothing_but_a_snapshot_is_allowed_past_the_input_bound() {
        let big = framed(SNAPSHOT + 1, MAX_FRAME + 1);
        let refused = write_frame(&mut Vec::new(), &big).expect_err("an oversized input was written");
        assert_eq!(refused.kind(), ErrorKind::InvalidData);

        // And on the way back in, where the length is claimed rather than measured: hand-frame it, since
        // `write_frame` is exactly what refuses to produce this.
        let mut bytes = (big.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&big);
        let mut r = &bytes[..];
        let refused = read_frame(&mut r, &mut Vec::new()).expect_err("an oversized input was read");
        assert_eq!(refused.kind(), ErrorKind::InvalidData);
    }

    /// A length past even the snapshot bound is refused on its length alone, before any of it is read — the
    /// four-byte template peek is only reached for lengths a snapshot could legitimately have.
    #[test]
    fn a_length_past_every_bound_is_refused_without_reading_the_frame() {
        let bytes = ((MAX_SNAPSHOT_FRAME + 1) as u32).to_le_bytes();
        let mut r = &bytes[..]; // nothing at all follows the length
        let refused = read_frame(&mut r, &mut Vec::new()).expect_err("a garbage length was accepted");
        assert_eq!(refused.kind(), ErrorKind::InvalidData, "it tried to read the frame before judging it");
    }
}
