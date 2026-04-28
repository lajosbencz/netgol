//! Per-chunk cycle detection. Pure compute; the server crate wraps this in a
//! tokio task that locks a shared [`Detector`] to feed reports and run scans.

use crate::world::ChunkCoord;
use crate::Chunk;
use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasherDefault;

use crate::world::CoordHasher;

pub const MAX_PERIOD: usize = 5;
const HISTORY: usize = MAX_PERIOD * 3;

#[derive(Debug, Clone)]
pub struct Oscillator {
    pub chunk: Chunk,
    pub period: u8,
    pub paused_at_tick: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct HashReport {
    pub coord: ChunkCoord,
    pub hash: u64,
    pub halo_was_zero: bool,
    pub tick: u64,
}

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
    in_pending: bool,
    last_tick: u64,
}

#[derive(Debug, Default)]
pub struct Detector {
    rings: HashMap<ChunkCoord, Ring, BuildHasherDefault<CoordHasher>>,
    pending: VecDeque<ChunkCoord>,
}

impl Detector {
    pub fn new() -> Self { Self::default() }

    pub fn observe(&mut self, report: HashReport) {
        let coord = report.coord;
        let r = self.rings.entry(coord).or_default();
        if !report.halo_was_zero {
            r.head = 0;
            r.filled = 0;
            r.poisoned = true;
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
        if (r.filled as usize) == HISTORY && !r.in_pending {
            r.in_pending = true;
            self.pending.push_back(coord);
        }
    }

    pub fn forget(&mut self, coord: ChunkCoord) {
        self.rings.remove(&coord);
    }

    /// Pop up to `budget` queued rings, emit promote requests for any that
    /// match a period in `1..=MAX_PERIOD`. A successfully-promoted ring is
    /// dropped (the chunk will be paused and stop reporting; if the sim
    /// refuses, observe() recreates the ring).
    pub fn scan(&mut self, budget: usize, out: &mut Vec<PromoteRequest>) {
        let mut taken = 0;
        while taken < budget {
            let coord = match self.pending.pop_front() {
                Some(c) => c,
                None => break,
            };
            let ring = match self.rings.get_mut(&coord) {
                Some(r) => r,
                None => continue,
            };
            if !ring.in_pending {
                continue;
            }
            ring.in_pending = false;
            if ring.poisoned || (ring.filled as usize) < HISTORY {
                taken += 1;
                continue;
            }
            if let Some(period) = detect_period(ring) {
                out.push(PromoteRequest { coord, period });
                self.rings.remove(&coord);
            }
            taken += 1;
        }
    }

    pub fn len(&self) -> usize { self.rings.len() }
    pub fn is_empty(&self) -> bool { self.rings.is_empty() }
    pub fn pending_len(&self) -> usize { self.pending.len() }
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
    fn promote_self_prunes_ring() {
        let mut d = Detector::new();
        for t in 0..HISTORY as u64 {
            d.observe(rep((0, 0), 42, t));
        }
        let mut out = Vec::new();
        d.scan(64, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(d.len(), 0, "promoted ring should be pruned");
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

    #[test]
    fn forget_removes_ring() {
        let mut d = Detector::new();
        for t in 0..HISTORY as u64 {
            d.observe(rep((0, 0), 42, t));
        }
        assert_eq!(d.len(), 1);
        d.forget((0, 0));
        assert_eq!(d.len(), 0);
        let mut out = Vec::new();
        d.scan(64, &mut out);
        assert!(out.is_empty(), "scan after forget yields nothing");
    }
}
