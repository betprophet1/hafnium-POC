//! A simulated sequencer for one group (spec §17): gap-free sequence numbers, time that never runs backwards,
//! stamping in place, and the sequencer's own inputs.

use venue_engine::PROTOCOL_VERSION;
use venue_journal::engine_started_codec::EngineStartedEncoder;
use venue_journal::message_header_codec::ENCODED_LENGTH as HDR;
use venue_journal::time_tick_codec::TimeTickEncoder;
use venue_journal::*;

pub struct Sequencer {
    group: u16,
    last_sequence: u64,
    last_at: u64,
}

impl Sequencer {
    /// Continues a group whose journal ends at `last_sequence`, stamped `last_at` (both 0 for a new journal).
    pub const fn resume(group: u16, last_sequence: u64, last_at: u64) -> Self {
        Self { group, last_sequence, last_at }
    }

    /// E-17.1 and E-17.2: stamps the input's `RecordHeader` in place and changes no other byte. `clock` is
    /// nanoseconds since the Unix epoch. Returns the sequence and time stamped.
    pub fn stamp(&mut self, input: &mut [u8], clock: u64) -> (u64, u64) {
        let sequence = self.last_sequence + 1;
        let at = if self.last_sequence == 0 { clock } else { clock.max(self.last_at + 1) };
        input[HDR..HDR + 8].copy_from_slice(&sequence.to_le_bytes());
        input[HDR + 8..HDR + 16].copy_from_slice(&at.to_le_bytes());
        input[HDR + 16..HDR + 18].copy_from_slice(&self.group.to_le_bytes());
        self.last_sequence = sequence;
        self.last_at = at;
        (sequence, at)
    }
}

/// An unstamped `TimeTick` (E-17.3).
pub fn time_tick() -> Vec<u8> {
    let mut buf = vec![0u8; 64];
    let enc = TimeTickEncoder::default().wrap(WriteBuf::new(&mut buf), HDR);
    let len = enc.header(0).parent().expect("a TimeTick fits in 64 bytes").get_limit();
    buf.truncate(len);
    buf
}

/// An unstamped `EngineStarted` naming this node and binary (E-17.3).
pub fn engine_started(node_id: u16, binary_hash: &[u8; 32]) -> Vec<u8> {
    let mut buf = vec![0u8; 128];
    let mut enc = EngineStartedEncoder::default().wrap(WriteBuf::new(&mut buf), HDR);
    enc = enc.header(0).parent().expect("an EngineStarted fits in 128 bytes");
    enc.node_id(node_id).protocol_version(PROTOCOL_VERSION);
    enc.binary_hash(binary_hash);
    let len = enc.get_limit();
    buf.truncate(len);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_gap_free_with_time_that_never_runs_backwards() {
        let mut s = Sequencer::resume(3, 0, 0);
        let mut a = time_tick();
        let untouched = a[HDR + 18..].to_vec();
        assert_eq!(s.stamp(&mut a, 1_000), (1, 1_000));
        assert_eq!(&a[HDR + 18..], &untouched[..]);
        let mut b = time_tick();
        assert_eq!(s.stamp(&mut b, 900), (2, 1_001), "a clock behind the last stamp is moved on by 1 ns");
        assert_eq!(u16::from_le_bytes([b[HDR + 16], b[HDR + 17]]), 3);
        let mut resumed = Sequencer::resume(3, 2, 1_001);
        assert_eq!(resumed.stamp(&mut time_tick(), 5_000), (3, 5_000));
    }
}
