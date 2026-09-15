//! Flood limits (blueprint §12): generic cell rate buckets in the gateway's own memory, on its monotonic clock.
//! Nothing refused here reaches the journal.

use std::time::{Duration, Instant};

/// The edge's clock. The workspace bans clocks because the engine's only clock is the journal; the gateway sits
/// before the journal, and nothing it measures is sequenced.
#[allow(clippy::disallowed_methods)]
pub fn monotonic() -> Instant {
    Instant::now()
}

pub struct Bucket {
    interval: Duration,
    tolerance: Duration,
    theoretical_arrival: Option<Instant>,
}

impl Bucket {
    fn new(period: Duration, count: u32, burst: u32) -> Self {
        let interval = period / count.max(1);
        Self { interval, tolerance: interval * burst.max(1), theoretical_arrival: None }
    }

    pub fn per_second(rate: u32, burst: u32) -> Self {
        Self::new(Duration::from_secs(1), rate, burst)
    }

    pub fn per_minute(rate: u32, burst: u32) -> Self {
        Self::new(Duration::from_secs(60), rate, burst)
    }

    /// Takes a cell at `at`, or says how long until one is free.
    pub fn take(&mut self, at: Instant) -> Result<(), Duration> {
        let next = self.theoretical_arrival.map_or(at, |t| t.max(at)) + self.interval;
        let limit = at + self.tolerance;
        if next > limit {
            return Err(next - limit);
        }
        self.theoretical_arrival = Some(next);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bucket_allows_its_burst_then_its_rate() {
        let t0 = monotonic();
        let mut bucket = Bucket::per_second(10, 3);
        for _ in 0..3 {
            assert!(bucket.take(t0).is_ok());
        }
        assert_eq!(bucket.take(t0), Err(Duration::from_millis(100)));
        assert!(bucket.take(t0 + Duration::from_millis(100)).is_ok());
        assert!(bucket.take(t0 + Duration::from_millis(100)).is_err());
        assert!(bucket.take(t0 + Duration::from_secs(5)).is_ok());
    }
}
