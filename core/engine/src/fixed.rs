//! Fixed-capacity open-addressed hash map over caller-provided storage.
//!
//! Linear probing with backward-shift deletion, kept at most half full so probes stay short and terminate.
//! Nothing here allocates. Iteration order is never exposed: callers that need to enumerate collect keys and
//! sort them (spec E-1.1, blueprint §4).

/// A key the engine can place in a [`FixedMap`]. `hash` must depend only on the key and the seed.
pub trait MapKey: Copy + Eq {
    fn hash(&self, seed: u64) -> u64;
}

/// SplitMix64 finaliser: a cheap, well-distributed mix of one word.
#[inline]
pub const fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl MapKey for u64 {
    #[inline]
    fn hash(&self, seed: u64) -> u64 {
        mix(*self ^ seed)
    }
}

impl MapKey for u32 {
    #[inline]
    fn hash(&self, seed: u64) -> u64 {
        mix(u64::from(*self) ^ seed)
    }
}

impl MapKey for (u64, u64) {
    #[inline]
    fn hash(&self, seed: u64) -> u64 {
        mix(mix(self.0 ^ seed) ^ self.1)
    }
}

impl MapKey for (u64, u32) {
    #[inline]
    fn hash(&self, seed: u64) -> u64 {
        mix(mix(self.0 ^ seed) ^ u64::from(self.1))
    }
}

impl<const N: usize> MapKey for [u8; N] {
    #[inline]
    fn hash(&self, seed: u64) -> u64 {
        let mut h = mix(seed ^ N as u64);
        for chunk in self.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            h = mix(h ^ u64::from_le_bytes(word));
        }
        h
    }
}

impl<const N: usize> MapKey for (u64, [u8; N]) {
    #[inline]
    fn hash(&self, seed: u64) -> u64 {
        self.1.hash(mix(seed ^ self.0))
    }
}

/// The map is at its load limit or its storage is unusable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full;

pub struct FixedMap<'a, K, V> {
    slots: &'a mut [Option<(K, V)>],
    len: usize,
    seed: u64,
}

impl<'a, K: MapKey, V> FixedMap<'a, K, V> {
    /// Wraps storage whose length is a power of two, clearing it.
    pub fn new(slots: &'a mut [Option<(K, V)>], seed: u64) -> Result<Self, Full> {
        if slots.is_empty() || !slots.len().is_power_of_two() {
            return Err(Full);
        }
        for s in slots.iter_mut() {
            *s = None;
        }
        Ok(Self { slots, len: 0, seed })
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Entries the map accepts before refusing inserts: half its slots.
    #[inline]
    pub fn limit(&self) -> usize {
        self.slots.len() / 2
    }

    #[inline]
    fn mask(&self) -> usize {
        self.slots.len() - 1
    }

    #[inline]
    fn home(&self, key: &K) -> usize {
        (key.hash(self.seed) as usize) & self.mask()
    }

    fn find(&self, key: &K) -> Option<usize> {
        let mut i = self.home(key);
        loop {
            match &self.slots[i] {
                None => return None,
                Some((k, _)) if k == key => return Some(i),
                Some(_) => i = (i + 1) & self.mask(),
            }
        }
    }

    pub fn contains(&self, key: &K) -> bool {
        self.find(key).is_some()
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.find(key).and_then(|i| self.slots[i].as_ref().map(|(_, v)| v))
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let i = self.find(key)?;
        self.slots[i].as_mut().map(|(_, v)| v)
    }

    /// Inserts or replaces. A new key is refused once the map holds [`limit`](Self::limit) entries.
    pub fn insert(&mut self, key: K, value: V) -> Result<Option<V>, Full> {
        let mut i = self.home(&key);
        loop {
            match &mut self.slots[i] {
                Some((k, v)) if *k == key => return Ok(Some(core::mem::replace(v, value))),
                Some(_) => i = (i + 1) & self.mask(),
                None => {
                    if self.len >= self.limit() {
                        return Err(Full);
                    }
                    self.slots[i] = Some((key, value));
                    self.len += 1;
                    return Ok(None);
                }
            }
        }
    }

    /// Removes a key, shifting later entries of its probe run back so lookups never cross a hole.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let mut hole = self.find(key)?;
        let (_, value) = self.slots[hole].take()?;
        self.len -= 1;
        let mask = self.mask();
        let mut j = (hole + 1) & mask;
        loop {
            let home = match &self.slots[j] {
                None => break,
                Some((k, _)) => self.home(k),
            };
            // Move the entry at j into the hole unless its home lies cyclically in (hole, j].
            let stays = if hole <= j { hole < home && home <= j } else { hole < home || home <= j };
            if !stays {
                self.slots[hole] = self.slots[j].take();
                hole = j;
            }
            j = (j + 1) & mask;
        }
        Some(value)
    }

    /// Visits every entry, in storage order. Never use the order for anything observable.
    pub fn for_each(&self, mut f: impl FnMut(&K, &V)) {
        for (k, v) in self.slots.iter().flatten() {
            f(k, v);
        }
    }

    /// Removes every entry for which `keep` returns false.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let mut i = 0;
        while i < self.slots.len() {
            let drop = matches!(&self.slots[i], Some((k, v)) if !keep(k, v));
            if drop {
                let key = self.slots[i].as_ref().map(|(k, _)| *k);
                if let Some(key) = key {
                    self.remove(&key);
                    // A backward shift may have moved an unvisited entry into slot i; look at it again.
                    continue;
                }
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_get_remove_and_limit() {
        let mut storage = [None; 16];
        let mut m: FixedMap<u64, u32> = FixedMap::new(&mut storage, 7).unwrap();
        for k in 0..8u64 {
            assert_eq!(m.insert(k * 1000, k as u32), Ok(None));
        }
        assert_eq!(m.insert(99, 1), Err(Full));
        assert_eq!(m.insert(3000, 33), Ok(Some(3)));
        assert_eq!(m.get(&3000), Some(&33));
        assert_eq!(m.remove(&3000), Some(33));
        assert_eq!(m.get(&3000), None);
        for k in [0u64, 1, 2, 4, 5, 6, 7] {
            assert_eq!(m.get(&(k * 1000)), Some(&(k as u32)), "key {k} lost after removal");
        }
    }

    #[test]
    fn backward_shift_keeps_every_key_reachable() {
        // Adversarial churn against a small table: every remaining key must stay findable.
        let mut storage = [None; 64];
        let mut m: FixedMap<u64, u64> = FixedMap::new(&mut storage, 1).unwrap();
        let mut present = [false; 200];
        let mut x = 12345u64;
        for _ in 0..20_000 {
            x = mix(x);
            let k = x % 200;
            if present[k as usize] {
                assert_eq!(m.remove(&k), Some(k * 3));
                present[k as usize] = false;
            } else if m.insert(k, k * 3).is_ok() {
                present[k as usize] = true;
            }
            for (key, &p) in present.iter().enumerate() {
                assert_eq!(m.get(&(key as u64)).is_some(), p);
            }
        }
    }

    #[test]
    fn retain_removes_exactly_the_rejected() {
        let mut storage = [None; 32];
        let mut m: FixedMap<u64, u64> = FixedMap::new(&mut storage, 9).unwrap();
        for k in 0..16u64 {
            m.insert(k, k).unwrap();
        }
        m.retain(|_, v| v % 3 != 0);
        for k in 0..16u64 {
            assert_eq!(m.contains(&k), k % 3 != 0);
        }
        assert_eq!(m.len(), 10);
    }
}
