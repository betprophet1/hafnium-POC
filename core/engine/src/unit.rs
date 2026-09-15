//! Change tracking for the blocks of a unit that report final values (spec E-5.2 blocks 3 to 5).
//!
//! Before a position, sleeve or book level changes, the engine records its key and its current value. At the
//! end of the unit the log is sorted by key and first record, so the value recorded first for each key is its
//! start-of-unit value; keys whose final value differs are reported in key order. The log lives in
//! caller-provided storage and is cleared for every unit.

/// The touch log ran out of room: a fail-stop `Capacity`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogFull;

pub struct Touches<'a, K, V> {
    log: &'a mut [(K, u32, V)],
    len: usize,
}

impl<'a, K: Ord + Copy, V: Copy + PartialEq> Touches<'a, K, V> {
    pub fn new(log: &'a mut [(K, u32, V)]) -> Self {
        Self { log, len: 0 }
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The records so far, in the order they were made.
    pub fn entries(&self) -> &[(K, u32, V)] {
        &self.log[..self.len]
    }

    /// Records `key` with its value before a change. Repeated touches are harmless.
    pub fn touch(&mut self, key: K, value: V) -> Result<(), LogFull> {
        if self.len > 0 && self.log[self.len - 1].0 == key {
            return Ok(());
        }
        let slot = self.log.get_mut(self.len).ok_or(LogFull)?;
        *slot = (key, self.len as u32, value);
        self.len += 1;
        Ok(())
    }

    /// Calls `changed(key, start, final)` for each touched key whose final value differs from its start value, in
    /// ascending key order, then clears the log.
    pub fn finish<E>(
        &mut self,
        mut current: impl FnMut(&K) -> V,
        mut changed: impl FnMut(&K, V, V) -> Result<(), E>,
    ) -> Result<(), E> {
        let log = &mut self.log[..self.len];
        log.sort_unstable_by_key(|e| (e.0, e.1));
        let mut i = 0;
        while i < log.len() {
            let (key, _, start) = log[i];
            let now = current(&key);
            if now != start {
                changed(&key, start, now)?;
            }
            while i < log.len() && log[i].0 == key {
                i += 1;
            }
        }
        self.len = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_changed_keys_once_in_key_order_with_first_values() {
        let mut storage = [(0u64, 0u32, 0i64); 16];
        let mut t = Touches::new(&mut storage);
        let mut state = [0i64; 4];
        // key 3 changes twice, key 1 changes and changes back, key 2 changes.
        t.touch(3, state[3]).unwrap();
        state[3] = 5;
        t.touch(1, state[1]).unwrap();
        state[1] = 7;
        t.touch(3, state[3]).unwrap();
        state[3] = 9;
        t.touch(2, state[2]).unwrap();
        state[2] = -1;
        t.touch(1, state[1]).unwrap();
        state[1] = 0;
        let mut seen = [(0u64, 0i64); 4];
        let mut n = 0;
        t.finish(
            |k| state[*k as usize],
            |k, _, v| -> Result<(), ()> {
                seen[n] = (*k, v);
                n += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(&seen[..n], &[(2, -1), (3, 9)]);
        assert!(t.is_empty());
    }

    #[test]
    fn a_full_log_is_reported() {
        let mut storage = [(0u8, 0u32, 0u8); 2];
        let mut t = Touches::new(&mut storage);
        t.touch(1, 0).unwrap();
        t.touch(2, 0).unwrap();
        assert_eq!(t.touch(3, 0), Err(LogFull));
    }
}
