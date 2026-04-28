//! Per-chunk cycle detection. Pure compute; the server crate wraps this in a
//! tokio task and feeds it [`HashReport`]s drained from each tick.

use crate::world::ChunkCoord;
use crate::Chunk;
use std::collections::HashMap;
use std::hash::BuildHasherDefault;

use crate::world::CoordHasher;

pub const MAX_PERIOD: usize = 5;
const HISTORY: usize = MAX_PERIOD * 3;

/// Detected oscillator stored on the world. Cell bits are the same bits that
/// lived in `chunks` before pausing - just relocated.
#[derive(Debug, Clone)]
pub struct Oscillator {
    pub chunk: Chunk,
    pub period: u8,
    pub paused_at_tick: u64,
}

/// Per-stepped-chunk record sent from the sim to the detector.
#[derive(Debug, Clone, Copy)]
pub struct HashReport {
    pub coord: ChunkCoord,
    pub hash: u64,
    pub halo_was_zero: bool,
    pub tick: u64,
}

/// Detector verdict: chunk is eligible for promotion to oscillating with this
/// period. The sim re-checks halo-zero on receipt before actually pausing.
#[derive(Debug, Clone, Copy)]
pub struct PromoteRequest {
    pub coord: ChunkCoord,
    pub period: u8,
}

#[derive(Debug, Default)]
struct Ring {
    hashes: [u64; HISTORY],
    head: u8,
    filled: u8,
    poisoned: bool,
    last_tick: u64,
    last_scanned_at_filled: u8,
}

#[derive(Debug, Default)]
pub struct Detector {
    rings: HashMap<ChunkCoord, Ring, BuildHasherDefault<CoordHasher>>,
    coord_buf: Vec<ChunkCoord>,
    cursor: usize,
}

impl Detector {
    pub fn new() -> Self { Self::default() }

    pub fn observe(&mut self, report: HashReport) {
        let r = self.rings.entry(report.coord).or_default();
        if !report.halo_was_zero {
            r.head = 0;
            r.filled = 0;
            r.poisoned = true;
            r.last_scanned_at_filled = 0;
            r.last_tick = report.tick;
            return;
        }
        if r.poisoned {
            r.poisoned = false;
        }
        r.hashes[r.head as usize] = report.hash;
        r.head = ((r.head as usize + 1) % HISTORY) as u8;
        if (r.filled as usize) < HISTORY {
            r.filled += 1;
        }
        r.last_tick = report.tick;
    }

    pub fn forget(&mut self, coord: ChunkCoord) {
        self.rings.remove(&coord);
    }

    /// Walk up to `budget` rings round-robin, emit promote requests for any
    /// ring that has filled with new data since its last scan and shows a
    /// period in `1..=MAX_PERIOD`.
    pub fn scan(&mut self, budget: usize, out: &mut Vec<PromoteRequest>) {
        if self.rings.is_empty() || budget == 0 {
            return;
        }
        self.coord_buf.clear();
        self.coord_buf.extend(self.rings.keys().copied());
        let n = self.coord_buf.len();
        let start = if n == 0 { 0 } else { self.cursor % n };
        let take = n.min(budget);
        for i in 0..take {
            let coord = self.coord_buf[(start + i) % n];
            let ring = match self.rings.get_mut(&coord) {
                Some(r) => r,
                None => continue,
            };
            if ring.poisoned {
                continue;
            }
            if (ring.filled as usize) < HISTORY {
                continue;
            }
            if ring.filled == ring.last_scanned_at_filled {
                continue;
            }
            ring.last_scanned_at_filled = ring.filled;
            if let Some(period) = detect_period(ring) {
                out.push(PromoteRequest { coord, period });
            }
        }
        self.cursor = if n == 0 { 0 } else { (start + take) % n };
    }

    pub fn len(&self) -> usize { self.rings.len() }
    pub fn is_empty(&self) -> bool { self.rings.is_empty() }
}

fn detect_period(ring: &Ring) -> Option<u8> {
    let mut ordered = [0u64; HISTORY];
    for i in 0..HISTORY {
        ordered[i] = ring.hashes[(ring.head as usize + i) % HISTORY];
    }
    for p in 1..=MAX_PERIOD {
        if HISTORY < 2 * p {
            continue;
        }
        let mut ok = true;
        for i in 0..(2 * p) {
            if ordered[HISTORY - 1 - i] != ordered[HISTORY - 1 - i - p] {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(p as u8);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(coord: ChunkCoord, hash: u64, tick: u64) -> HashReport {
        HashReport { coord, hash, halo_was_zero: true, tick }
    }

    #[test]
    fn detects_still_life_period_1() {
        let mut d = Detector::new();
        for t in 0..HISTORY as u64 {
            d.observe(rep((0, 0), 42, t));
        }
        let mut out = Vec::new();
        d.scan(64, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].period, 1);
    }

    #[test]
    fn detects_blinker_period_2() {
        let mut d = Detector::new();
        for t in 0..HISTORY as u64 {
            let h = if t % 2 == 0 { 100 } else { 200 };
            d.observe(rep((0, 0), h, t));
        }
        let mut out = Vec::new();
        d.scan(64, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].period, 2);
    }

    #[test]
    fn detects_period_3() {
        let mut d = Detector::new();
        let cycle = [11u64, 22, 33];
        for t in 0..HISTORY as u64 {
            d.observe(rep((0, 0), cycle[(t as usize) % 3], t));
        }
        let mut out = Vec::new();
        d.scan(64, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].period, 3);
    }

    #[test]
    fn poisoned_by_nonzero_halo() {
        let mut d = Detector::new();
        for t in 0..HISTORY as u64 {
            d.observe(rep((0, 0), 42, t));
        }
        d.observe(HashReport { coord: (0, 0), hash: 42, halo_was_zero: false, tick: HISTORY as u64 });
        let mut out = Vec::new();
        d.scan(64, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn budget_caps_scan() {
        let mut d = Detector::new();
        for c in 0..10 {
            for t in 0..HISTORY as u64 {
                d.observe(rep((c, 0), 99, t));
            }
        }
        let mut out = Vec::new();
        d.scan(3, &mut out);
        assert_eq!(out.len(), 3);
        out.clear();
        d.scan(3, &mut out);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn no_repeat_promote_without_new_data() {
        let mut d = Detector::new();
        for t in 0..HISTORY as u64 {
            d.observe(rep((0, 0), 42, t));
        }
        let mut out = Vec::new();
        d.scan(64, &mut out);
        assert_eq!(out.len(), 1);
        out.clear();
        d.scan(64, &mut out);
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn random_pattern_does_not_falsely_promote() {
        let mut d = Detector::new();
        let mut x = 0xdead_beef_u64;
        for t in 0..HISTORY as u64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            d.observe(rep((0, 0), x, t));
        }
        let mut out = Vec::new();
        d.scan(64, &mut out);
        assert!(out.is_empty());
    }
}
