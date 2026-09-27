// Dense vs. sparse+overflow representation for a book's price levels, for ADR-0001 (docs/adr).
//
// Dense mirrors what the engine actually does today (core/engine/src/state.rs's `levels`/`level_bits`,
// illustrated as `Book100` in blueprint/prediction-exchange.html and prototypes/maskbench): one array slot
// per tick, permanently reserved for the book's whole tick-table width, plus an occupancy bitmap.
//
// Sparse holds a handful of occupied ticks inline with the book itself, spilling into a shared `OverflowPool`
// once inline capacity is exceeded. The pool is an intrusive singly-linked free list — one head scalar per
// book, `next` threaded through the pool's own entries — the same pattern core/engine/src/book.rs's order
// slab already uses (`alloc_order`/`free_slot`), not a Vec per book: a Vec's own header costs ~96 bytes
// (4 fields x ptr+len+cap) whether or not anything is in it, which would understate a cold book's real
// footprint and defeat the point of measuring one.
//
// What this answers: at what occupied-tick count does inline+overflow scanning stop being competitive with
// the dense array's O(1) bitmap lookup, and how much memory does each representation actually cost at a given
// occupancy. Both numbers feed ADR-0001's open questions (inline capacity K, promotion threshold).

use std::hint::black_box;
use std::mem::size_of;
use std::time::{Duration, Instant};

const TICKS: usize = 100;
const NONE: u32 = u32::MAX;

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
}

fn distinct(rng: &mut Rng, n: usize, lo: usize, hi: usize) -> Vec<usize> {
    let mut v: Vec<usize> = Vec::new();
    while v.len() < n {
        let x = lo + (rng.next() as usize) % (hi - lo);
        if !v.contains(&x) {
            v.push(x);
        }
    }
    v.sort();
    v
}

fn time_ns_per_op(ops_per_rep: usize, mut f: impl FnMut() -> u64) -> f64 {
    let mut sink = 0u64;
    for _ in 0..1000 {
        sink = sink.wrapping_add(f());
    }
    let (t0, mut reps) = (Instant::now(), 0u64);
    while t0.elapsed() < Duration::from_millis(300) {
        for _ in 0..256 {
            sink = sink.wrapping_add(f());
        }
        reps += 256;
    }
    black_box(sink);
    t0.elapsed().as_nanos() as f64 / (reps as f64 * ops_per_rep.max(1) as f64)
}

/// The engine's actual shape today: one slot per tick, an occupancy bitmap.
#[derive(Clone)]
struct DenseBook {
    qty: [u32; TICKS],
    head: [u32; TICKS],
    tail: [u32; TICKS],
    bits: u128,
}
impl DenseBook {
    fn empty() -> Self {
        DenseBook { qty: [0; TICKS], head: [NONE; TICKS], tail: [NONE; TICKS], bits: 0 }
    }
    fn insert(&mut self, tick: usize, qty: u32) {
        self.qty[tick] = qty;
        self.head[tick] = tick as u32;
        self.tail[tick] = tick as u32;
        self.bits |= 1u128 << tick;
    }
    fn remove(&mut self, tick: usize) {
        self.qty[tick] = 0;
        self.bits &= !(1u128 << tick);
    }
    #[inline(never)]
    fn best_below(&self, p: usize) -> Option<usize> {
        let m = self.bits & ((1u128 << p) - 1);
        if m == 0 {
            None
        } else {
            Some(127 - m.leading_zeros() as usize)
        }
    }
}

/// One shared, externally-owned pool of overflow tick entries: an intrusive singly-linked free list, exactly
/// the pattern core/engine/src/book.rs's order slab already uses. `next` does double duty — the free chain
/// when a slot is unused, a book's own occupied chain when it isn't — so nothing beyond these five arrays is
/// ever allocated.
struct OverflowPool {
    tick: Vec<u16>,
    qty: Vec<u32>,
    head: Vec<u32>,
    tail: Vec<u32>,
    next: Vec<u32>,
    free: u32,
}
impl OverflowPool {
    fn with_capacity(n: usize) -> Self {
        let next: Vec<u32> = (0..n).map(|i| if i + 1 < n { i as u32 + 1 } else { NONE }).collect();
        OverflowPool { tick: vec![0; n], qty: vec![0; n], head: vec![0; n], tail: vec![0; n], next, free: 0 }
    }
    fn alloc(&mut self) -> u32 {
        let slot = self.free;
        assert_ne!(slot, NONE, "overflow pool exhausted — raise its capacity for this benchmark");
        self.free = self.next[slot as usize];
        slot
    }
    fn free_slot(&mut self, slot: u32) {
        self.next[slot as usize] = self.free;
        self.free = slot;
    }
}

/// A handful of occupied ticks held inline with the book; `overflow_head` is `NONE` until inline capacity is
/// exceeded. Fixed size regardless of how much (if anything) has spilled into the shared pool.
#[derive(Clone, Copy)]
struct SparseBook<const INLINE: usize> {
    tick: [u16; INLINE],
    qty: [u32; INLINE],
    head: [u32; INLINE],
    tail: [u32; INLINE],
    len: u8,
    overflow_head: u32,
}
impl<const INLINE: usize> SparseBook<INLINE> {
    fn empty() -> Self {
        SparseBook {
            tick: [0; INLINE],
            qty: [0; INLINE],
            head: [0; INLINE],
            tail: [0; INLINE],
            len: 0,
            overflow_head: NONE,
        }
    }
    fn insert(&mut self, pool: &mut OverflowPool, tick: usize, qty: u32) {
        if (self.len as usize) < INLINE {
            let i = self.len as usize;
            self.tick[i] = tick as u16;
            self.qty[i] = qty;
            self.head[i] = tick as u32;
            self.tail[i] = tick as u32;
            self.len += 1;
        } else {
            let slot = pool.alloc();
            pool.tick[slot as usize] = tick as u16;
            pool.qty[slot as usize] = qty;
            pool.head[slot as usize] = tick as u32;
            pool.tail[slot as usize] = tick as u32;
            pool.next[slot as usize] = self.overflow_head;
            self.overflow_head = slot;
        }
    }
    fn remove(&mut self, pool: &mut OverflowPool, tick: usize) {
        let t = tick as u16;
        let n = self.len as usize;
        if let Some(i) = self.tick[..n].iter().position(|&x| x == t) {
            let last = n - 1;
            self.tick[i] = self.tick[last];
            self.qty[i] = self.qty[last];
            self.head[i] = self.head[last];
            self.tail[i] = self.tail[last];
            self.len = last as u8;
            return;
        }
        let (mut prev, mut cur) = (NONE, self.overflow_head);
        while cur != NONE {
            if pool.tick[cur as usize] == t {
                let nxt = pool.next[cur as usize];
                if prev == NONE {
                    self.overflow_head = nxt;
                } else {
                    pool.next[prev as usize] = nxt;
                }
                pool.free_slot(cur);
                return;
            }
            prev = cur;
            cur = pool.next[cur as usize];
        }
    }
    #[inline(never)]
    fn best_below(&self, pool: &OverflowPool, p: usize) -> Option<usize> {
        let mut best: Option<usize> = None;
        for i in 0..self.len as usize {
            let t = self.tick[i] as usize;
            if t < p && best.is_none_or(|b| t > b) {
                best = Some(t);
            }
        }
        let mut cur = self.overflow_head;
        while cur != NONE {
            let t = pool.tick[cur as usize] as usize;
            if t < p && best.is_none_or(|b| t > b) {
                best = Some(t);
            }
            cur = pool.next[cur as usize];
        }
        best
    }
}

/// Sparse -> dense: copies each occupied tick's (qty, head, tail) into the dense array and sets its bitmap
/// bit. Never touches an `Order` record itself — a real conversion only relocates these small per-tick
/// triples, the same thing `compact_levels()` already does dense-to-dense in `core/engine/src/commands.rs`,
/// just between two different shapes here instead of within one.
fn promote<const INLINE: usize>(sparse: &SparseBook<INLINE>, pool: &OverflowPool) -> DenseBook {
    let mut dense = DenseBook::empty();
    for i in 0..sparse.len as usize {
        let t = sparse.tick[i] as usize;
        dense.qty[t] = sparse.qty[i];
        dense.head[t] = sparse.head[i];
        dense.tail[t] = sparse.tail[i];
        dense.bits |= 1u128 << t;
    }
    let mut cur = sparse.overflow_head;
    while cur != NONE {
        let t = pool.tick[cur as usize] as usize;
        dense.qty[t] = pool.qty[cur as usize];
        dense.head[t] = pool.head[cur as usize];
        dense.tail[t] = pool.tail[cur as usize];
        dense.bits |= 1u128 << t;
        cur = pool.next[cur as usize];
    }
    dense
}

/// Dense -> sparse: walks the occupancy bitmap directly (never a 0..100 scan), placing each occupied tick
/// inline until capacity, then spilling the rest into the shared pool. Demotion should only ever be attempted
/// on a book already known (by the density check) to fit; if it doesn't, that's a design bug in the caller,
/// not a normal runtime condition — this asserts rather than degrading silently, the same fail-stop instinct
/// the real engine applies to any other table that turns out to be too small.
fn demote<const INLINE: usize>(dense: &DenseBook, pool: &mut OverflowPool) -> SparseBook<INLINE> {
    let mut sparse = SparseBook::<INLINE>::empty();
    let mut bits = dense.bits;
    while bits != 0 {
        let t = bits.trailing_zeros() as usize;
        bits &= bits - 1;
        if (sparse.len as usize) < INLINE {
            let i = sparse.len as usize;
            sparse.tick[i] = t as u16;
            sparse.qty[i] = dense.qty[t];
            sparse.head[i] = dense.head[t];
            sparse.tail[i] = dense.tail[t];
            sparse.len += 1;
        } else {
            let slot = pool.alloc();
            pool.tick[slot as usize] = t as u16;
            pool.qty[slot as usize] = dense.qty[t];
            pool.head[slot as usize] = dense.head[t];
            pool.tail[slot as usize] = dense.tail[t];
            pool.next[slot as usize] = sparse.overflow_head;
            sparse.overflow_head = slot;
        }
    }
    sparse
}

/// Returns every overflow slot a sparse book holds to the pool's free list. Used only to make repeated
/// `demote` benchmark reps safe to run back-to-back without exhausting the pool — not part of the real
/// design, where a demoted book simply keeps whatever overflow slots it was given.
fn release<const INLINE: usize>(sparse: &SparseBook<INLINE>, pool: &mut OverflowPool) {
    let mut cur = sparse.overflow_head;
    while cur != NONE {
        let nxt = pool.next[cur as usize];
        pool.free_slot(cur);
        cur = nxt;
    }
}

fn build_dense(levels: &[usize]) -> DenseBook {
    let mut b = DenseBook::empty();
    for &l in levels {
        b.insert(l, 100);
    }
    b
}
fn build_sparse<const INLINE: usize>(pool: &mut OverflowPool, levels: &[usize]) -> SparseBook<INLINE> {
    let mut b = SparseBook::<INLINE>::empty();
    for &l in levels {
        b.insert(pool, l, 100);
    }
    b
}

fn row(name: &str, dense: f64, sparse: f64) {
    println!("{:<48} {:>10.2} ns {:>10.2} ns {:>8.1}x", name, dense, sparse, sparse / dense);
}

fn bench_best_below<const INLINE: usize>(pool_cap: usize, name: &str, levels: &[usize]) {
    let mut pool = OverflowPool::with_capacity(pool_cap);
    let dense = build_dense(levels);
    let sparse = build_sparse::<INLINE>(&mut pool, levels);
    for p in 0..=TICKS {
        assert_eq!(dense.best_below(p), sparse.best_below(&pool, p), "mismatch at p={p} for {name}");
    }
    let d = time_ns_per_op(TICKS, || {
        let mut a = 0u64;
        for p in 1..=TICKS {
            a += black_box(&dense).best_below(black_box(p)).unwrap_or(0) as u64;
        }
        a
    });
    let s = time_ns_per_op(TICKS, || {
        let mut a = 0u64;
        for p in 1..=TICKS {
            a += black_box(&sparse).best_below(black_box(&pool), black_box(p)).unwrap_or(0) as u64;
        }
        a
    });
    row(name, d, s);
}

fn bench_insert_remove<const INLINE: usize>(pool_cap: usize, name: &str, levels: &[usize], touch: usize) {
    // insert(touch) + remove(touch) is self-reversing (touch is never already occupied), so the same book and
    // pool can be mutated in place across every timed rep — no per-iteration reset or copy needed, and none of
    // that reset cost leaks into the measurement the way a per-rep clone/rebuild would.
    let mut base_d = build_dense(levels);
    let d = time_ns_per_op(1, || {
        base_d.insert(touch, 50);
        base_d.remove(touch);
        base_d.qty.iter().map(|&q| q as u64).sum()
    });
    let mut pool = OverflowPool::with_capacity(pool_cap);
    let mut base_s = build_sparse::<INLINE>(&mut pool, levels);
    let s = time_ns_per_op(1, || {
        base_s.insert(&mut pool, touch, 50);
        base_s.remove(&mut pool, touch);
        base_s.qty.iter().map(|&q| q as u64).sum::<u64>()
    });
    row(name, d, s);
}

fn conv_row(name: &str, ns: f64) {
    println!("{name:<48} {ns:>10.2} ns");
}

fn bench_promote<const INLINE: usize>(pool_cap: usize, name: &str, levels: &[usize]) {
    let mut pool = OverflowPool::with_capacity(pool_cap);
    let sparse = build_sparse::<INLINE>(&mut pool, levels);
    let reference = build_dense(levels);
    let promoted = promote(&sparse, &pool);
    for p in 0..=TICKS {
        assert_eq!(reference.best_below(p), promoted.best_below(p), "promote mismatch at p={p} for {name}");
    }
    let ns = time_ns_per_op(1, || promote(black_box(&sparse), black_box(&pool)).bits as u64);
    conv_row(name, ns);
}

fn bench_demote<const INLINE: usize>(pool_cap: usize, name: &str, levels: &[usize]) {
    let dense = build_dense(levels);
    let mut ref_pool = OverflowPool::with_capacity(pool_cap);
    let reference = build_sparse::<INLINE>(&mut ref_pool, levels);
    let mut pool = OverflowPool::with_capacity(pool_cap);
    // demote + release is a size-preserving (if not byte-identical) round trip on the pool's free list, the
    // same way insert+remove was for bench_insert_remove — so one pool survives every timed rep below.
    let check = demote::<INLINE>(&dense, &mut pool);
    for p in 0..=TICKS {
        assert_eq!(
            reference.best_below(&ref_pool, p),
            check.best_below(&pool, p),
            "demote mismatch at p={p} for {name}"
        );
    }
    release(&check, &mut pool);
    let ns = time_ns_per_op(1, || {
        let s = demote::<INLINE>(black_box(&dense), &mut pool);
        let n = s.len as u64;
        release(&s, &mut pool);
        n
    });
    conv_row(name, ns);
}

fn mem_row(name: &str, bytes: usize) {
    println!("{name:<48} {bytes:>10} bytes");
}

fn main() {
    let mut rng = Rng(0xD1B54A32D192ED03);

    println!("Fixed per-book memory cost (independent of occupancy — this is the ADR-0001 space side of the trade)\n");
    mem_row("DenseBook (100 ticks, today's mechanism)", size_of::<DenseBook>());
    mem_row("SparseBook<4> (inline shell only)", size_of::<SparseBook<4>>());
    mem_row("SparseBook<8> (inline shell only)", size_of::<SparseBook<8>>());
    mem_row("SparseBook<16> (inline shell only)", size_of::<SparseBook<16>>());
    mem_row("one OverflowPool entry (amortised, shared)", size_of::<u16>() + size_of::<u32>() * 4);
    println!();

    println!("{:<48} {:>13} {:>13} {:>9}", "best_below, all p in 1..=100", "dense (ns/op)", "sparse (ns/op)", "ratio");
    bench_best_below::<4>(64, "1 occupied tick, INLINE=4", &distinct(&mut rng, 1, 1, 99));
    bench_best_below::<4>(64, "3 occupied ticks, INLINE=4", &distinct(&mut rng, 3, 1, 99));
    bench_best_below::<4>(64, "4 occupied ticks, INLINE=4 (at capacity)", &distinct(&mut rng, 4, 1, 99));
    bench_best_below::<4>(64, "8 occupied ticks, INLINE=4 (4 in overflow)", &distinct(&mut rng, 8, 1, 99));
    bench_best_below::<4>(64, "16 occupied ticks, INLINE=4 (12 in overflow)", &distinct(&mut rng, 16, 1, 99));
    bench_best_below::<8>(64, "8 occupied ticks, INLINE=8 (at capacity)", &distinct(&mut rng, 8, 1, 99));
    bench_best_below::<8>(64, "16 occupied ticks, INLINE=8 (8 in overflow)", &distinct(&mut rng, 16, 1, 99));
    bench_best_below::<8>(64, "32 occupied ticks, INLINE=8 (24 in overflow)", &distinct(&mut rng, 32, 1, 99));
    bench_best_below::<8>(96, "64 occupied ticks, INLINE=8 (56 in overflow)", &distinct(&mut rng, 64, 1, 99));
    bench_best_below::<8>(96, "99 occupied ticks (fully dense), INLINE=8", &(0..99).collect::<Vec<_>>());
    println!();

    println!("{:<48} {:>13} {:>13} {:>9}", "insert+remove one new tick", "dense (ns/op)", "sparse (ns/op)", "ratio");
    bench_insert_remove::<4>(64, "into 1 occupied, INLINE=4", &distinct(&mut rng, 1, 1, 90), 95);
    bench_insert_remove::<4>(64, "into 8 occupied, INLINE=4 (overflow)", &distinct(&mut rng, 8, 1, 90), 95);
    bench_insert_remove::<8>(64, "into 8 occupied, INLINE=8 (at capacity)", &distinct(&mut rng, 8, 1, 90), 95);
    bench_insert_remove::<8>(64, "into 32 occupied, INLINE=8 (overflow)", &distinct(&mut rng, 32, 1, 90), 95);
    println!();

    println!("{:<48} {:>13}", "promote sparse -> dense (one full conversion)", "ns/op");
    bench_promote::<4>(64, "1 occupied tick, INLINE=4", &distinct(&mut rng, 1, 1, 99));
    bench_promote::<4>(64, "4 occupied ticks, INLINE=4 (at capacity)", &distinct(&mut rng, 4, 1, 99));
    bench_promote::<4>(64, "8 occupied ticks, INLINE=4 (4 in overflow)", &distinct(&mut rng, 8, 1, 99));
    bench_promote::<8>(64, "8 occupied ticks, INLINE=8 (at capacity)", &distinct(&mut rng, 8, 1, 99));
    bench_promote::<8>(96, "32 occupied ticks, INLINE=8 (24 in overflow)", &distinct(&mut rng, 32, 1, 99));
    bench_promote::<8>(96, "64 occupied ticks, INLINE=8 (56 in overflow)", &distinct(&mut rng, 64, 1, 99));
    println!();

    println!("{:<48} {:>13}", "demote dense -> sparse (one full conversion)", "ns/op");
    bench_demote::<4>(64, "1 occupied tick, INLINE=4", &distinct(&mut rng, 1, 1, 99));
    bench_demote::<4>(64, "4 occupied ticks, INLINE=4 (at capacity)", &distinct(&mut rng, 4, 1, 99));
    bench_demote::<4>(64, "8 occupied ticks, INLINE=4 (4 in overflow)", &distinct(&mut rng, 8, 1, 99));
    bench_demote::<8>(64, "8 occupied ticks, INLINE=8 (at capacity)", &distinct(&mut rng, 8, 1, 99));
    bench_demote::<8>(96, "32 occupied ticks, INLINE=8 (24 in overflow)", &distinct(&mut rng, 32, 1, 99));
    bench_demote::<8>(96, "64 occupied ticks, INLINE=8 (56 in overflow)", &distinct(&mut rng, 64, 1, 99));

    println!("\ncorrectness: sparse best_below matched dense for every query position across every scenario above,");
    println!("and every promote/demote round-trip matched an independently-built reference book too");
}
