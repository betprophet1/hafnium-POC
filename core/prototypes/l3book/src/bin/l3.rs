// L3 book in the §6 layout: fixed order slab + free list, id->slot open addressing (backward-shift delete),
// per-tick intrusive doubly linked FIFO, L2 aggregates and occupancy masks on top. Differential-tested.
use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;
const NIL: u32 = u32::MAX;
#[derive(Clone, Copy)]
struct Order {
    id: u64,
    qty: u32,
    price: u8,
    side: u8,
    prev: u32,
    next: u32,
} // side 0 bid, 1 ask

struct IdMap {
    keys: Vec<u64>,
    vals: Vec<u32>,
    mask: usize,
    shift: u32,
} // key 0 = empty
impl IdMap {
    fn new(cap_pow2: usize) -> Self {
        IdMap {
            keys: vec![0; cap_pow2],
            vals: vec![0; cap_pow2],
            mask: cap_pow2 - 1,
            shift: 64 - cap_pow2.trailing_zeros(),
        }
    }
    #[inline]
    fn home(&self, k: u64) -> usize {
        (k.wrapping_mul(0x9E3779B97F4A7C15) >> self.shift) as usize
    }
    fn insert(&mut self, k: u64, v: u32) {
        let mut i = self.home(k);
        while self.keys[i] != 0 {
            i = (i + 1) & self.mask;
        }
        self.keys[i] = k;
        self.vals[i] = v;
    }
    fn find(&self, k: u64) -> Option<usize> {
        let mut i = self.home(k);
        while self.keys[i] != 0 {
            if self.keys[i] == k {
                return Some(i);
            }
            i = (i + 1) & self.mask;
        }
        None
    }
    fn get(&self, k: u64) -> Option<u32> {
        self.find(k).map(|i| self.vals[i])
    }
    fn remove(&mut self, k: u64) {
        // backward-shift: no tombstones, no rehash
        let mut i = self.find(k).expect("remove of absent id");
        let mut j = i;
        loop {
            j = (j + 1) & self.mask;
            if self.keys[j] == 0 {
                break;
            }
            let h = self.home(self.keys[j]);
            let in_range = if i <= j {
                i < h && h <= j
            } else {
                i < h || h <= j
            };
            if !in_range {
                self.keys[i] = self.keys[j];
                self.vals[i] = self.vals[j];
                i = j;
            }
        }
        self.keys[i] = 0;
    }
}

struct Book {
    slab: Vec<Order>,
    free: u32,
    live: usize,
    ids: IdMap,
    level_qty: [u32; 100],
    level_head: [u32; 100],
    level_tail: [u32; 100],
    bid_mask: u128,
    ask_mask: u128,
}
impl Book {
    fn new(cap: usize) -> Self {
        let mut slab = vec![
            Order {
                id: 0,
                qty: 0,
                price: 0,
                side: 0,
                prev: NIL,
                next: NIL
            };
            cap
        ];
        for i in 0..cap {
            slab[i].next = if i + 1 < cap { (i + 1) as u32 } else { NIL };
        } // free list threaded through `next`
        Book {
            slab,
            free: 0,
            live: 0,
            ids: IdMap::new((cap * 2).next_power_of_two()),
            level_qty: [0; 100],
            level_head: [NIL; 100],
            level_tail: [NIL; 100],
            bid_mask: 0,
            ask_mask: 0,
        }
    }
    fn mask(&mut self, side: u8) -> &mut u128 {
        if side == 0 {
            &mut self.bid_mask
        } else {
            &mut self.ask_mask
        }
    }

    // add in the back: O(1) via the tail index
    fn rest(&mut self, id: u64, side: u8, price: u8, qty: u32) -> bool {
        if self.free == NIL {
            return false;
        } // capacity is a sized, deterministic limit
        let s = self.free;
        self.free = self.slab[s as usize].next;
        self.live += 1;
        let p = price as usize;
        let t = self.level_tail[p];
        self.slab[s as usize] = Order {
            id,
            qty,
            price,
            side,
            prev: t,
            next: NIL,
        };
        if t != NIL {
            self.slab[t as usize].next = s;
        } else {
            self.level_head[p] = s;
            *self.mask(side) |= 1u128 << p;
        }
        self.level_tail[p] = s;
        self.level_qty[p] += qty;
        self.ids.insert(id, s);
        true
    }
    // unlink from anywhere in the FIFO: O(1) via prev/next
    fn unlink(&mut self, s: u32) {
        let o = self.slab[s as usize];
        let p = o.price as usize;
        if o.prev != NIL {
            self.slab[o.prev as usize].next = o.next;
        } else {
            self.level_head[p] = o.next;
        }
        if o.next != NIL {
            self.slab[o.next as usize].prev = o.prev;
        } else {
            self.level_tail[p] = o.prev;
        }
        if self.level_head[p] == NIL {
            *self.mask(o.side) &= !(1u128 << p);
        }
        self.ids.remove(o.id);
        self.slab[s as usize].next = self.free;
        self.free = s;
        self.live -= 1;
    }
    fn cancel(&mut self, id: u64) -> bool {
        match self.ids.get(id) {
            None => false,
            Some(s) => {
                let q = self.slab[s as usize].qty;
                self.level_qty[self.slab[s as usize].price as usize] -= q;
                self.unlink(s);
                true
            }
        }
    }
    // size reduction keeps queue position; an increase or a price change is cancel + new (goes to the back)
    fn reduce(&mut self, id: u64, new_qty: u32) -> bool {
        let Some(s) = self.ids.get(id) else {
            return false;
        };
        let o = &mut self.slab[s as usize];
        if new_qty >= o.qty {
            return false;
        }
        if new_qty == 0 {
            return self.cancel(id);
        }
        let d = o.qty - new_qty;
        o.qty = new_qty;
        let p = o.price as usize;
        self.level_qty[p] -= d;
        true
    }
    // match from the front of the best level, then rest any remainder
    fn submit(
        &mut self,
        id: u64,
        side: u8,
        price: u8,
        mut qty: u32,
        fills: &mut Vec<(u64, u8, u32)>,
    ) -> bool {
        loop {
            if qty == 0 {
                return true;
            }
            let best = if side == 0 {
                if self.ask_mask == 0 {
                    break;
                }
                self.ask_mask.trailing_zeros() as u8
            } else {
                if self.bid_mask == 0 {
                    break;
                }
                (127 - self.bid_mask.leading_zeros()) as u8
            };
            if (side == 0 && best > price) || (side == 1 && best < price) {
                break;
            }
            let p = best as usize;
            while qty > 0 && self.level_head[p] != NIL {
                let h = self.level_head[p];
                let take = qty.min(self.slab[h as usize].qty);
                fills.push((self.slab[h as usize].id, best, take));
                qty -= take;
                self.slab[h as usize].qty -= take;
                self.level_qty[p] -= take;
                if self.slab[h as usize].qty == 0 {
                    self.unlink(h);
                }
            }
        }
        self.rest(id, side, price, qty)
    }
    fn dump(&self) -> Vec<(u8, u8, Vec<(u64, u32)>)> {
        let mut out = vec![];
        for p in 1..100usize {
            let mut v = vec![];
            let mut s = self.level_head[p];
            let mut side = 9;
            let mut sum = 0;
            let mut prev = NIL;
            while s != NIL {
                let o = self.slab[s as usize];
                assert_eq!(o.prev, prev, "prev link");
                side = o.side;
                v.push((o.id, o.qty));
                sum += o.qty;
                assert_eq!(self.ids.get(o.id), Some(s), "id map");
                prev = s;
                s = o.next;
            }
            assert_eq!(self.level_tail[p], prev, "tail");
            assert_eq!(self.level_qty[p], sum, "level qty");
            let bit = |m: u128| (m >> p) & 1 == 1;
            assert_eq!(
                bit(self.bid_mask) || bit(self.ask_mask),
                !v.is_empty(),
                "mask vs list"
            );
            assert!(
                !(bit(self.bid_mask) && bit(self.ask_mask)),
                "tick holds both sides"
            );
            if !v.is_empty() {
                assert_eq!(
                    if side == 0 {
                        bit(self.bid_mask)
                    } else {
                        bit(self.ask_mask)
                    },
                    true
                );
                out.push((p as u8, side, v));
            }
        }
        if self.bid_mask != 0 && self.ask_mask != 0 {
            assert!(
                127 - self.bid_mask.leading_zeros() < self.ask_mask.trailing_zeros(),
                "crossed book"
            );
        }
        out
    }
}

// Reference model: ordered map of FIFO queues. Slow and obviously correct.
#[derive(Default)]
struct Model {
    bids: BTreeMap<u8, VecDeque<(u64, u32)>>,
    asks: BTreeMap<u8, VecDeque<(u64, u32)>>,
    live: usize,
}
impl Model {
    fn submit(
        &mut self,
        id: u64,
        side: u8,
        price: u8,
        mut qty: u32,
        fills: &mut Vec<(u64, u8, u32)>,
        cap: usize,
    ) -> bool {
        loop {
            if qty == 0 {
                return true;
            }
            let opp = if side == 0 {
                &mut self.asks
            } else {
                &mut self.bids
            };
            let best = if side == 0 {
                opp.keys().next().copied()
            } else {
                opp.keys().next_back().copied()
            };
            let Some(b) = best else { break };
            if (side == 0 && b > price) || (side == 1 && b < price) {
                break;
            }
            let q = opp.get_mut(&b).unwrap();
            while qty > 0 {
                let Some(f) = q.front_mut() else { break };
                let t = qty.min(f.1);
                fills.push((f.0, b, t));
                qty -= t;
                f.1 -= t;
                if f.1 == 0 {
                    q.pop_front();
                    self.live -= 1;
                }
            }
            if q.is_empty() {
                opp.remove(&b);
            }
        }
        if self.live >= cap {
            return false;
        }
        (if side == 0 {
            &mut self.bids
        } else {
            &mut self.asks
        })
        .entry(price)
        .or_default()
        .push_back((id, qty));
        self.live += 1;
        true
    }
    fn find(&mut self, id: u64) -> Option<(u8, u8, usize)> {
        for (side, m) in [(0u8, &self.bids), (1u8, &self.asks)] {
            for (p, q) in m {
                if let Some(i) = q.iter().position(|o| o.0 == id) {
                    return Some((side, *p, i));
                }
            }
        }
        None
    }
    fn cancel(&mut self, id: u64) -> bool {
        let Some((side, p, i)) = self.find(id) else {
            return false;
        };
        let m = if side == 0 {
            &mut self.bids
        } else {
            &mut self.asks
        };
        let q = m.get_mut(&p).unwrap();
        q.remove(i);
        if q.is_empty() {
            m.remove(&p);
        }
        self.live -= 1;
        true
    }
    fn reduce(&mut self, id: u64, nq: u32) -> bool {
        let Some((side, p, i)) = self.find(id) else {
            return false;
        };
        let m = if side == 0 {
            &mut self.bids
        } else {
            &mut self.asks
        };
        let o = &mut m.get_mut(&p).unwrap()[i];
        if nq >= o.1 {
            return false;
        }
        if nq == 0 {
            return self.cancel(id);
        }
        o.1 = nq;
        true
    }
    fn dump(&self) -> Vec<(u8, u8, Vec<(u64, u32)>)> {
        let mut out: Vec<_> = self
            .bids
            .iter()
            .map(|(p, q)| (*p, 0u8, q.iter().copied().collect()))
            .chain(
                self.asks
                    .iter()
                    .map(|(p, q)| (*p, 1u8, q.iter().copied().collect())),
            )
            .collect();
        out.sort_by_key(|x| x.0);
        out
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn main() {
    const CAP: usize = 512;
    let (mut book, mut model, mut rng) = (Book::new(CAP), Model::default(), Rng(0xC0FFEE1234567));
    let (mut next_id, mut live_ids): (u64, Vec<u64>) = (1, vec![]);
    let (mut n_sub, mut n_can, mut n_red, mut n_fill, mut n_full) = (0, 0, 0, 0, 0);
    for step in 0..200_000u32 {
        let r = rng.below(100);
        if r < 55 || live_ids.is_empty() {
            let side = (rng.next() & 1) as u8;
            let price = (35 + rng.below(30)) as u8 + if side == 0 { 0 } else { 2 };
            let qty = 1 + rng.below(50) as u32;
            let (mut f1, mut f2) = (vec![], vec![]);
            let ok1 = book.submit(next_id, side, price, qty, &mut f1);
            let ok2 = model.submit(next_id, side, price, qty, &mut f2, CAP);
            assert_eq!(f1, f2, "fills differ at step {step}");
            assert_eq!(ok1, ok2, "capacity decision differs at step {step}");
            if !ok1 {
                n_full += 1;
            }
            n_fill += f1.len();
            n_sub += 1;
            live_ids.push(next_id);
            next_id += 1;
        } else if r < 85 {
            let k = rng.below(live_ids.len() as u64) as usize;
            let id = live_ids.swap_remove(k);
            assert_eq!(
                book.cancel(id),
                model.cancel(id),
                "cancel differs at step {step}"
            );
            n_can += 1;
        } else {
            let id = live_ids[rng.below(live_ids.len() as u64) as usize];
            let nq = rng.below(40) as u32;
            assert_eq!(
                book.reduce(id, nq),
                model.reduce(id, nq),
                "reduce differs at step {step}"
            );
            n_red += 1;
        }
        if step % 64 == 0 {
            live_ids.retain(|id| book.ids.get(*id).is_some());
        } // drop ids that filled
        assert_eq!(book.dump(), model.dump(), "L3 book differs at step {step}");
        assert_eq!(book.live, model.live);
    }
    println!("differential test: 200000 steps OK  ({n_sub} submits, {n_fill} fills, {n_can} cancels, {n_red} size reductions, {n_full} rejected at capacity {CAP})");
    println!("  checked after every step: identical L3 book and fills vs reference; prev/next/tail links; per-tick qty; mask==occupancy; no tick holds both sides; never crossed; id map exact");

    // timing, no checker
    let mut book = Book::new(1 << 16);
    let mut rng = Rng(42);
    let mut ids: Vec<u64> = vec![];
    let mut id = 1u64;
    let mut fills = Vec::with_capacity(1024);
    let n = 3_000_000;
    let t0 = Instant::now();
    for _ in 0..n {
        let r = rng.below(100);
        if r < 55 || ids.is_empty() {
            let side = (rng.next() & 1) as u8;
            let price = (35 + rng.below(30)) as u8 + side * 2;
            fills.clear();
            book.submit(id, side, price, 1 + rng.below(50) as u32, &mut fills);
            ids.push(id);
            id += 1;
        } else if r < 85 {
            let k = rng.below(ids.len() as u64) as usize;
            let x = ids.swap_remove(k);
            book.cancel(x);
        } else {
            let x = ids[rng.below(ids.len() as u64) as usize];
            book.reduce(x, rng.below(40) as u32);
        }
        if ids.len() > 200_000 {
            ids.retain(|x| book.ids.get(*x).is_some());
        }
    }
    let ns = t0.elapsed().as_nanos() as f64 / n as f64;
    println!("timing: {n} mixed ops (55% submit incl. crossing, 30% cancel, 15% reduce): {ns:.1} ns/op average, live orders at end {}", book.live);
}
