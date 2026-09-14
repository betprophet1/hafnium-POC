// Occupancy bitmap vs scanning the level array, for the operations a book actually performs.
use std::hint::black_box;
use std::time::{Duration, Instant};

struct Rng(u64);
impl Rng { fn next(&mut self) -> u64 { let mut x = self.0; x ^= x << 13; x ^= x >> 7; x ^= x << 17; self.0 = x; x } }

// 1-cent tick: 100 slots, one u128.
struct Book100 { qty: [u32; 100], bits: u128 }
impl Book100 {
    fn new(levels: &[usize]) -> Self { let mut b = Book100 { qty: [0; 100], bits: 0 }; for &l in levels { b.qty[l] = 100; b.bits |= 1u128 << l; } b }
    #[inline(never)] fn next_below_scan(&self, p: usize) -> Option<usize> { let mut i = p; while i > 0 { i -= 1; if self.qty[i] != 0 { return Some(i); } } None }
    #[inline(never)] fn next_below_bits(&self, p: usize) -> Option<usize> { let m = self.bits & ((1u128 << p) - 1); if m == 0 { None } else { Some(127 - m.leading_zeros() as usize) } }
    #[inline(never)] fn top5_scan(&self, out: &mut [usize; 5]) -> usize { let mut n = 0; let mut i = 100; while i > 0 && n < 5 { i -= 1; if self.qty[i] != 0 { out[n] = i; n += 1; } } n }
    #[inline(never)] fn top5_bits(&self, out: &mut [usize; 5]) -> usize { let mut m = self.bits; let mut n = 0; while m != 0 && n < 5 { let i = 127 - m.leading_zeros() as usize; out[n] = i; n += 1; m ^= 1u128 << i; } n }
}

// 0.1-cent tick: 1000 slots, 16 u64 words plus a u16 summary of non-empty words.
struct Book1000 { qty: [u32; 1000], words: [u64; 16], summary: u16 }
impl Book1000 {
    fn new(levels: &[usize]) -> Self { let mut b = Book1000 { qty: [0; 1000], words: [0; 16], summary: 0 }; for &l in levels { b.qty[l] = 100; b.words[l >> 6] |= 1u64 << (l & 63); b.summary |= 1u16 << (l >> 6); } b }
    #[inline(never)] fn next_below_scan(&self, p: usize) -> Option<usize> { let mut i = p; while i > 0 { i -= 1; if self.qty[i] != 0 { return Some(i); } } None }
    #[inline(never)] fn next_below_bits(&self, p: usize) -> Option<usize> {
        let (w, b) = (p >> 6, p & 63);
        let m = self.words[w] & (1u64 << b).wrapping_sub(1);
        if m != 0 { return Some((w << 6) + 63 - m.leading_zeros() as usize); }
        let s = (self.summary as u32) & ((1u32 << w) - 1);
        if s == 0 { return None; }
        let w2 = 31 - s.leading_zeros() as usize;
        Some((w2 << 6) + 63 - self.words[w2].leading_zeros() as usize)
    }
    #[inline(never)] fn top5_scan(&self, out: &mut [usize; 5]) -> usize { let mut n = 0; let mut i = 1000; while i > 0 && n < 5 { i -= 1; if self.qty[i] != 0 { out[n] = i; n += 1; } } n }
    #[inline(never)] fn top5_bits(&self, out: &mut [usize; 5]) -> usize {
        let (mut s, mut n) = (self.summary as u32, 0);
        while s != 0 && n < 5 {
            let w = 31 - s.leading_zeros() as usize; let mut m = self.words[w];
            while m != 0 && n < 5 { let b = 63 - m.leading_zeros() as usize; out[n] = (w << 6) + b; n += 1; m ^= 1u64 << b; }
            s ^= 1u32 << w;
        }
        n
    }
}

fn time_ns_per_op(ops_per_rep: usize, mut f: impl FnMut() -> u64) -> f64 {
    let mut sink = 0u64; for _ in 0..1000 { sink = sink.wrapping_add(f()); }
    let (t0, mut reps) = (Instant::now(), 0u64);
    while t0.elapsed() < Duration::from_millis(400) { for _ in 0..256 { sink = sink.wrapping_add(f()); } reps += 256; }
    black_box(sink);
    t0.elapsed().as_nanos() as f64 / (reps as f64 * ops_per_rep as f64)
}

fn distinct(rng: &mut Rng, n: usize, lo: usize, hi: usize) -> Vec<usize> {
    let mut v: Vec<usize> = Vec::new(); while v.len() < n { let x = lo + (rng.next() as usize) % (hi - lo); if !v.contains(&x) { v.push(x); } } v.sort(); v
}

fn row(name: &str, scan: f64, bits: f64) { println!("{:<46} {:>8.2} ns {:>8.2} ns {:>7.1}x", name, scan, bits, scan / bits); }

fn main() {
    let mut rng = Rng(0x9E3779B97F4A7C15);
    println!("{:<46} {:>11} {:>11} {:>8}", "operation / book shape", "array scan", "bitmap", "speedup");

    // next best after the best level empties: query from every occupied level (walking down the book)
    let cases100: [(&str, Vec<usize>); 2] = [
        ("next best, 1c HOT (16 adjacent bid levels)", (30..46).collect()),
        ("next best, 1c COLD (6 scattered levels)", distinct(&mut rng, 6, 1, 99)),
    ];
    for (name, lv) in cases100.iter() {
        let b = Book100::new(lv);
        for &p in lv { assert_eq!(b.next_below_scan(p), b.next_below_bits(p)); }
        for p in 0..100 { assert_eq!(b.next_below_scan(p), b.next_below_bits(p)); }
        let s = time_ns_per_op(lv.len(), || { let mut a = 0u64; for &p in lv { a += black_box(&b).next_below_scan(black_box(p)).unwrap_or(0) as u64; } a });
        let t = time_ns_per_op(lv.len(), || { let mut a = 0u64; for &p in lv { a += black_box(&b).next_below_bits(black_box(p)).unwrap_or(0) as u64; } a });
        row(name, s, t);
    }
    let cases1000: [(&str, Vec<usize>); 2] = [
        ("next best, 0.1c parlay (8 levels, deep OTM)", distinct(&mut rng, 8, 1, 300)),
        ("next best, 0.1c thin (3 levels anywhere)", distinct(&mut rng, 3, 1, 999)),
    ];
    for (name, lv) in cases1000.iter() {
        let b = Book1000::new(lv);
        for p in 0..1000 { assert_eq!(b.next_below_scan(p), b.next_below_bits(p), "p={}", p); }
        let s = time_ns_per_op(lv.len(), || { let mut a = 0u64; for &p in lv { a += black_box(&b).next_below_scan(black_box(p)).unwrap_or(0) as u64; } a });
        let t = time_ns_per_op(lv.len(), || { let mut a = 0u64; for &p in lv { a += black_box(&b).next_below_bits(black_box(p)).unwrap_or(0) as u64; } a });
        row(name, s, t);
    }
    // worst case for a scan: the book empties entirely below the old best
    {
        let lv = vec![900usize]; let b = Book1000::new(&lv);
        let s = time_ns_per_op(1, || black_box(&b).next_below_scan(black_box(900)).map_or(1, |x| x as u64));
        let t = time_ns_per_op(1, || black_box(&b).next_below_bits(black_box(900)).map_or(1, |x| x as u64));
        row("next best, 0.1c, last level just emptied", s, t);
    }
    // top-5 depth for a market-data snapshot
    {
        let lv: Vec<usize> = (30..46).collect(); let b = Book100::new(&lv);
        let (mut o1, mut o2) = ([0; 5], [0; 5]); let n1 = b.top5_scan(&mut o1); let n2 = b.top5_bits(&mut o2); assert_eq!((n1, o1), (n2, o2));
        let s = time_ns_per_op(1, || { let mut o = [0; 5]; black_box(&b).top5_scan(&mut o) as u64 });
        let t = time_ns_per_op(1, || { let mut o = [0; 5]; black_box(&b).top5_bits(&mut o) as u64 });
        row("top-5 depth, 1c HOT", s, t);
    }
    {
        let lv = distinct(&mut rng, 8, 1, 300); let b = Book1000::new(&lv);
        let (mut o1, mut o2) = ([0; 5], [0; 5]); let n1 = b.top5_scan(&mut o1); let n2 = b.top5_bits(&mut o2); assert_eq!((n1, o1), (n2, o2));
        let s = time_ns_per_op(1, || { let mut o = [0; 5]; black_box(&b).top5_scan(&mut o) as u64 });
        let t = time_ns_per_op(1, || { let mut o = [0; 5]; black_box(&b).top5_bits(&mut o) as u64 });
        row("top-5 depth, 0.1c parlay (8 levels, deep OTM)", s, t);
    }
    println!("correctness: bitmap answers matched the array scan for every query position");
}
