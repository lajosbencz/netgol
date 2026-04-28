//! Sparse chunk-indexed world. Stepping grows into neighbor chunks as edges birth cells,
//! and drops chunks that go fully empty unless they're frozen.

use crate::chunk::{Chunk, EdgeBundle, StepResult};
use crate::oscillator::{HashReport, Oscillator};
use crate::{CHUNK_SIZE, CHUNK_SIZE_I64};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

pub type ChunkCoord = (i32, i32);

/// Cheap multiply-mix hasher specialized for the `(i32, i32)` chunk coord. Std
/// `HashMap` uses SipHash for HashDoS resistance, which costs ~50 ops per write;
/// for purely internal hashing this brings that down to ~6 ops while keeping
/// reasonable distribution.
#[derive(Default)]
pub struct CoordHasher(u64);

impl Hasher for CoordHasher {
    #[inline]
    fn finish(&self) -> u64 { self.0 }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        // Cold path; tuples of i32 hit `write_i32` instead.
        for &b in bytes {
            self.0 = self.0.rotate_left(5) ^ u64::from(b);
            self.0 = self.0.wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
    #[inline]
    fn write_i32(&mut self, n: i32) {
        self.0 = self.0.rotate_left(13) ^ u64::from(n as u32);
        self.0 = self.0.wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

pub(crate) type CoordMap<V> = HashMap<ChunkCoord, V, BuildHasherDefault<CoordHasher>>;
type CoordSet = HashSet<ChunkCoord, BuildHasherDefault<CoordHasher>>;

#[derive(Debug, Default, Clone)]
pub struct World {
    chunks: CoordMap<Chunk>,
    oscillators: CoordMap<Oscillator>,
    tick: u64,
    /// Start-of-tick snapshot: halo assembly must not see mid-tick mutations
    /// from earlier candidates in the loop. Cleared (capacity retained) per tick.
    scratch_edges: CoordMap<EdgeBundle>,
    scratch_candidates: CoordSet,
    scratch_candidates_vec: Vec<ChunkCoord>,
    scratch_wakes: Vec<ChunkCoord>,
}

#[derive(Debug, Default, Clone)]
pub struct TickOutcome {
    pub changed: Vec<ChunkCoord>,
    pub removed: Vec<ChunkCoord>,
    pub hash_reports: Vec<HashReport>,
}

impl World {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tick_number(&self) -> u64 {
        self.tick
    }

    pub fn len(&self) -> usize {
        self.chunks.len() + self.oscillators.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty() && self.oscillators.is_empty()
    }

    pub fn get_chunk(&self, cx: i32, cy: i32) -> Option<&Chunk> {
        self.chunks
            .get(&(cx, cy))
            .or_else(|| self.oscillators.get(&(cx, cy)).map(|o| &o.chunk))
    }

    pub fn iter_chunks(&self) -> impl Iterator<Item = (ChunkCoord, &Chunk)> {
        self.chunks
            .iter()
            .map(|(c, ch)| (*c, ch))
            .chain(self.oscillators.iter().map(|(c, o)| (*c, &o.chunk)))
    }

    pub fn set_cell(&mut self, x: i64, y: i64, alive: bool) {
        let (coord, lx, ly) = split(x, y);
        let chunk = self.chunks.entry(coord).or_insert_with(Chunk::empty);
        chunk.set(lx, ly, alive);
    }

    pub fn freeze_cell(&mut self, x: i64, y: i64, alive: bool) {
        let (coord, lx, ly) = split(x, y);
        let chunk = self.chunks.entry(coord).or_insert_with(Chunk::empty);
        chunk.freeze(lx, ly, alive);
    }

    pub fn unfreeze_cell(&mut self, x: i64, y: i64) {
        let (coord, lx, ly) = split(x, y);
        if let Some(chunk) = self.chunks.get_mut(&coord) {
            chunk.unfreeze(lx, ly);
        }
    }

    /// Insert or replace a chunk wholesale (used by snapshot load).
    pub fn insert_chunk(&mut self, coord: ChunkCoord, chunk: Chunk) {
        self.chunks.insert(coord, chunk);
    }

    /// Remove a chunk (used by the reaper). Returns `true` if it existed in
    /// either `chunks` or `oscillators`.
    pub fn remove_chunk(&mut self, coord: ChunkCoord) -> bool {
        let in_chunks = self.chunks.remove(&coord).is_some();
        let in_oscillators = self.oscillators.remove(&coord).is_some();
        in_chunks || in_oscillators
    }

    pub fn set_tick_number(&mut self, tick: u64) {
        self.tick = tick;
    }

    pub fn oscillator_count(&self) -> usize {
        self.oscillators.len()
    }

    pub fn is_oscillating(&self, coord: ChunkCoord) -> bool {
        self.oscillators.contains_key(&coord)
    }

    /// Move a chunk from `chunks` to `oscillators`. Refuses if the chunk is
    /// missing, frozen, or its current halo is non-zero (the detector's verdict
    /// can be stale by the time the sim drains the request). Returns true on
    /// success.
    pub fn promote_oscillator(&mut self, coord: ChunkCoord, period: u8) -> bool {
        if period == 0 || (period as usize) > crate::oscillator::MAX_PERIOD {
            return false;
        }
        match self.chunks.get(&coord) {
            Some(c) if !c.is_frozen() => {}
            _ => return false,
        }
        if !self.halo_for(coord).is_zero() {
            return false;
        }
        let chunk = self.chunks.remove(&coord).expect("contains_key checked above");
        self.oscillators.insert(coord, Oscillator {
            chunk,
            period,
            paused_at_tick: self.tick,
        });
        true
    }

    fn halo_for(&self, coord: ChunkCoord) -> EdgeBundle {
        let (x, y) = coord;
        let edges_of = |c: ChunkCoord| {
            self.chunks.get(&c)
                .filter(|ch| !ch.is_empty())
                .map(|ch| ch.edges())
                .unwrap_or_else(EdgeBundle::empty)
        };
        let above = edges_of((x, y - 1));
        let below = edges_of((x, y + 1));
        let left = edges_of((x - 1, y));
        let right = edges_of((x + 1, y));
        let tl = edges_of((x - 1, y - 1));
        let tr = edges_of((x + 1, y - 1));
        let bl = edges_of((x - 1, y + 1));
        let br = edges_of((x + 1, y + 1));
        EdgeBundle {
            top: above.bottom,
            bottom: below.top,
            left: left.right,
            right: right.left,
            corners: [tl.corners[3], tr.corners[2], bl.corners[1], br.corners[0]],
        }
    }

    /// If `coord` is paused, advance it to the current tick's phase and put it
    /// back in `chunks`. Returns true if a wake happened.
    pub fn wake_if_paused(&mut self, coord: ChunkCoord) -> bool {
        if let Some(osc) = self.oscillators.remove(&coord) {
            let chunk = wake_chunk(osc, self.tick);
            self.chunks.insert(coord, chunk);
            true
        } else {
            false
        }
    }

    /// Advance every live chunk and its neighbors by one GoL step.
    ///
    /// Candidate selection is edge-aware: a neighbor chunk is only stepped if the
    /// live chunk's relevant edge has cells that could birth into it. Combined with
    /// the empty-chunk + zero-halo early-out in `Chunk::step`, this keeps the
    /// per-tick work proportional to *active* world activity, not chunk count.
    pub fn tick(&mut self) -> TickOutcome {
        let mut outcome = TickOutcome::default();
        self.tick_into(&mut outcome);
        outcome
    }

    /// Reusable-buffer variant of [`tick`]. Caller-owned `outcome` is cleared
    /// at entry; its `Vec` capacities survive across calls.
    pub fn tick_into(&mut self, outcome: &mut TickOutcome) {
        outcome.changed.clear();
        outcome.removed.clear();
        outcome.hash_reports.clear();
        self.scratch_edges.clear();
        self.scratch_candidates.clear();
        self.scratch_candidates_vec.clear();
        self.scratch_wakes.clear();

        for (&coord, ch) in &self.chunks {
            if ch.is_empty() {
                continue;
            }
            self.scratch_edges.insert(coord, ch.edges());
        }

        if !self.oscillators.is_empty() {
            for (&(x, y), e) in &self.scratch_edges {
                let probes: [(bool, ChunkCoord); 8] = [
                    (e.top != 0, (x, y - 1)),
                    (e.bottom != 0, (x, y + 1)),
                    (e.left != 0, (x - 1, y)),
                    (e.right != 0, (x + 1, y)),
                    (e.corners[0] != 0, (x - 1, y - 1)),
                    (e.corners[1] != 0, (x + 1, y - 1)),
                    (e.corners[2] != 0, (x - 1, y + 1)),
                    (e.corners[3] != 0, (x + 1, y + 1)),
                ];
                for (has_bit, c) in probes {
                    if has_bit && self.oscillators.contains_key(&c) {
                        self.scratch_wakes.push(c);
                    }
                }
            }
            self.scratch_wakes.sort_unstable();
            self.scratch_wakes.dedup();
            for &coord in &self.scratch_wakes {
                let osc = self.oscillators.remove(&coord).expect("contains_key probed above");
                let chunk = wake_chunk(osc, self.tick);
                self.scratch_edges.insert(coord, chunk.edges());
                self.chunks.insert(coord, chunk);
                outcome.changed.push(coord);
            }
        }

        let edges = &self.scratch_edges;
        let set = &mut self.scratch_candidates;
        let vec = &mut self.scratch_candidates_vec;
        let mut push = |c: ChunkCoord| {
            if set.insert(c) {
                vec.push(c);
            }
        };
        for (&(x, y), e) in edges {
            push((x, y));
            if e.top != 0 { push((x, y - 1)); }
            if e.bottom != 0 { push((x, y + 1)); }
            if e.left != 0 { push((x - 1, y)); }
            if e.right != 0 { push((x + 1, y)); }
            if e.corners[0] != 0 { push((x - 1, y - 1)); }
            if e.corners[1] != 0 { push((x + 1, y - 1)); }
            if e.corners[2] != 0 { push((x - 1, y + 1)); }
            if e.corners[3] != 0 { push((x + 1, y + 1)); }
        }

        let empty_chunk = Chunk::empty();
        let now_tick = self.tick;
        for i in 0..self.scratch_candidates_vec.len() {
            let coord = self.scratch_candidates_vec[i];
            let halo_was_zero;
            let result = {
                let current = self.chunks.get(&coord).unwrap_or(&empty_chunk);
                let halo = assemble_halo(coord, &self.scratch_edges);
                halo_was_zero = halo.is_zero();
                current.step(&halo)
            };
            let next = match result {
                StepResult::Unchanged => {
                    outcome.hash_reports.push(HashReport {
                        coord,
                        hash: self.chunks.get(&coord).map(Chunk::hash_state).unwrap_or(0),
                        halo_was_zero,
                        tick: now_tick,
                    });
                    continue;
                }
                StepResult::Stepped(c) => c,
            };
            if next.is_empty() && !next.is_frozen() {
                if self.chunks.remove(&coord).is_some() {
                    outcome.removed.push(coord);
                }
                continue;
            }
            let chunk_hash = next.hash_state();
            match self.chunks.entry(coord) {
                Entry::Occupied(mut slot) => {
                    if slot.get().rows() != next.rows() {
                        slot.insert(next);
                        outcome.changed.push(coord);
                    }
                }
                Entry::Vacant(slot) => {
                    slot.insert(next);
                    outcome.changed.push(coord);
                }
            }
            outcome.hash_reports.push(HashReport {
                coord,
                hash: chunk_hash,
                halo_was_zero,
                tick: now_tick,
            });
        }

        self.tick = self.tick.checked_add(1).expect("tick counter overflow");
    }
}

fn split(x: i64, y: i64) -> (ChunkCoord, usize, usize) {
    let cx = x.div_euclid(CHUNK_SIZE_I64);
    let cy = y.div_euclid(CHUNK_SIZE_I64);
    let cx = i32::try_from(cx).expect("chunk x out of i32 range");
    let cy = i32::try_from(cy).expect("chunk y out of i32 range");
    let lx = x.rem_euclid(CHUNK_SIZE_I64) as usize;
    let ly = y.rem_euclid(CHUNK_SIZE_I64) as usize;
    debug_assert!(lx < CHUNK_SIZE && ly < CHUNK_SIZE);
    ((cx, cy), lx, ly)
}

fn wake_chunk(osc: Oscillator, current_tick: u64) -> Chunk {
    let skipped = current_tick.saturating_sub(osc.paused_at_tick);
    let phase_offset = (skipped % u64::from(osc.period)) as u8;
    let mut chunk = osc.chunk;
    let empty = EdgeBundle::empty();
    for _ in 0..phase_offset {
        chunk = match chunk.step(&empty) {
            StepResult::Stepped(c) => c,
            StepResult::Unchanged => chunk,
        };
    }
    chunk
}

fn assemble_halo(coord: ChunkCoord, cache: &CoordMap<EdgeBundle>) -> EdgeBundle {
    let (x, y) = coord;
    let empty = EdgeBundle::empty();
    let above = cache.get(&(x, y - 1)).copied().unwrap_or(empty);
    let below = cache.get(&(x, y + 1)).copied().unwrap_or(empty);
    let left = cache.get(&(x - 1, y)).copied().unwrap_or(empty);
    let right = cache.get(&(x + 1, y)).copied().unwrap_or(empty);
    let tl = cache.get(&(x - 1, y - 1)).copied().unwrap_or(empty);
    let tr = cache.get(&(x + 1, y - 1)).copied().unwrap_or(empty);
    let bl = cache.get(&(x - 1, y + 1)).copied().unwrap_or(empty);
    let br = cache.get(&(x + 1, y + 1)).copied().unwrap_or(empty);
    EdgeBundle {
        top: above.bottom,
        bottom: below.top,
        left: left.right,
        right: right.left,
        corners: [tl.corners[3], tr.corners[2], bl.corners[1], br.corners[0]],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn glider_drifts_southeast() {
        // Standard glider at origin. After 4 ticks it has translated (+1, +1).
        let mut w = World::new();
        for &(x, y) in &[(1, 0), (2, 1), (0, 2), (1, 2), (2, 2)] {
            w.set_cell(x, y, true);
        }
        let initial: BTreeSet<(i64, i64)> = collect_live(&w);
        for _ in 0..4 {
            w.tick();
        }
        let after: BTreeSet<(i64, i64)> = collect_live(&w);
        let translated: BTreeSet<(i64, i64)> = initial.iter().map(|(x, y)| (x + 1, y + 1)).collect();
        assert_eq!(after, translated, "glider did not translate (+1,+1) in 4 ticks");
        assert_eq!(w.tick_number(), 4);
    }

    #[test]
    fn glider_crosses_chunk_boundary() {
        let mut w = World::new();
        // Place near positive boundary so it walks into the next chunk.
        let bx = (CHUNK_SIZE as i64) - 4;
        let by = (CHUNK_SIZE as i64) - 4;
        for &(x, y) in &[(1, 0), (2, 1), (0, 2), (1, 2), (2, 2)] {
            w.set_cell(bx + x, by + y, true);
        }
        for _ in 0..16 {
            w.tick();
        }
        // Glider should have drifted +(4,4) - comfortably across the chunk seam.
        assert!(w.iter_chunks().count() >= 1);
        let live = collect_live(&w);
        assert_eq!(live.len(), 5);
    }

    #[test]
    fn r_pentomino_grows_and_stabilizes_count() {
        // R-pentomino chaos test: just check it doesn't panic and population grows.
        let mut w = World::new();
        for &(x, y) in &[(1, 0), (2, 0), (0, 1), (1, 1), (1, 2)] {
            w.set_cell(x, y, true);
        }
        let initial = total_live(&w);
        for _ in 0..50 {
            w.tick();
        }
        assert!(total_live(&w) > initial);
    }

    #[test]
    fn empty_chunks_are_dropped() {
        let mut w = World::new();
        // Lone cell dies on tick 1; chunk should be removed.
        w.set_cell(0, 0, true);
        assert_eq!(w.len(), 1);
        w.tick();
        assert_eq!(w.len(), 0);
    }

    #[test]
    fn frozen_chunk_is_kept_when_empty() {
        let mut w = World::new();
        w.freeze_cell(0, 0, false);
        w.tick();
        assert_eq!(w.len(), 1);
        assert!(w.get_chunk(0, 0).unwrap().is_frozen());
    }

    fn collect_live(w: &World) -> BTreeSet<(i64, i64)> {
        let mut out = BTreeSet::new();
        for ((cx, cy), ch) in w.iter_chunks() {
            for y in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    if ch.get(x, y) {
                        out.insert((
                            (cx as i64) * CHUNK_SIZE_I64 + x as i64,
                            (cy as i64) * CHUNK_SIZE_I64 + y as i64,
                        ));
                    }
                }
            }
        }
        out
    }

    fn total_live(w: &World) -> u32 {
        w.iter_chunks().map(|(_, c)| c.live_count()).sum()
    }

    fn place_blinker(w: &mut World, ox: i64, oy: i64) {
        for &(x, y) in &[(0, 1), (1, 1), (2, 1)] {
            w.set_cell(ox + x, oy + y, true);
        }
    }

    fn run_until_promoted(
        w: &mut World,
        det: &mut crate::oscillator::Detector,
        coord: ChunkCoord,
        max_ticks: u64,
    ) -> u8 {
        let mut outcome = TickOutcome::default();
        let mut promote_buf = Vec::new();
        for _ in 0..max_ticks {
            w.tick_into(&mut outcome);
            for r in outcome.hash_reports.drain(..) {
                det.observe(r);
            }
            promote_buf.clear();
            det.scan(64, &mut promote_buf);
            for req in promote_buf.drain(..) {
                if req.coord == coord && w.promote_oscillator(req.coord, req.period) {
                    return req.period;
                }
            }
            if w.is_oscillating(coord) {
                return 0;
            }
        }
        panic!("chunk not promoted within {max_ticks} ticks");
    }

    #[test]
    fn blinker_is_paused_after_detection() {
        let mut w = World::new();
        place_blinker(&mut w, 5, 5);
        let mut det = crate::oscillator::Detector::new();
        let period = run_until_promoted(&mut w, &mut det, (0, 0), 200);
        assert_eq!(period, 2, "blinker should be detected as period-2");
        assert!(w.is_oscillating((0, 0)));
        assert!(w.get_chunk(0, 0).is_some(), "paused chunk still visible via get_chunk");
        let live_before = total_live(&w);
        let mut outcome = TickOutcome::default();
        for _ in 0..50 {
            w.tick_into(&mut outcome);
            assert!(outcome.changed.is_empty(), "paused chunk emitted a change event");
        }
        assert!(w.is_oscillating((0, 0)));
        assert_eq!(total_live(&w), live_before);
    }

    #[test]
    fn block_is_paused_as_period_one() {
        let mut w = World::new();
        for &(x, y) in &[(2, 2), (3, 2), (2, 3), (3, 3)] {
            w.set_cell(x, y, true);
        }
        let mut det = crate::oscillator::Detector::new();
        let period = run_until_promoted(&mut w, &mut det, (0, 0), 200);
        assert_eq!(period, 1, "block (still life) should be period 1");
        assert!(w.is_oscillating((0, 0)));
    }

    #[test]
    fn paused_blinker_woken_by_glider_matches_plain_run() {
        // Two parallel worlds: one with detection, one without. Same starting
        // state - blinker in (0,0), glider entering from a neighbor chunk.
        // After all ticks both worlds must hold identical live cells.
        let mut paused = World::new();
        let mut plain = World::new();
        place_blinker(&mut paused, 5, 5);
        place_blinker(&mut plain, 5, 5);
        // Glider in chunk (-1, 0), heading southeast toward (0, 0).
        let glider_origin = (-(CHUNK_SIZE as i64) + 50, 30);
        for &(x, y) in &[(1, 0), (2, 1), (0, 2), (1, 2), (2, 2)] {
            paused.set_cell(glider_origin.0 + x, glider_origin.1 + y, true);
            plain.set_cell(glider_origin.0 + x, glider_origin.1 + y, true);
        }
        let mut det = crate::oscillator::Detector::new();
        run_until_promoted(&mut paused, &mut det, (0, 0), 50);
        assert!(paused.is_oscillating((0, 0)));
        let mut outcome = TickOutcome::default();
        let plain_target_tick = paused.tick_number() + 100;
        while plain.tick_number() < plain_target_tick {
            plain.tick();
        }
        for _ in 0..100 {
            paused.tick_into(&mut outcome);
            for r in outcome.hash_reports.drain(..) {
                det.observe(r);
            }
        }
        assert_eq!(paused.tick_number(), plain.tick_number());
        assert_eq!(collect_live(&paused), collect_live(&plain), "paused/plain diverged");
    }

    #[test]
    fn edit_wakes_paused_chunk() {
        let mut w = World::new();
        place_blinker(&mut w, 5, 5);
        let mut det = crate::oscillator::Detector::new();
        run_until_promoted(&mut w, &mut det, (0, 0), 200);
        assert!(w.is_oscillating((0, 0)));
        assert!(w.wake_if_paused((0, 0)));
        assert!(!w.is_oscillating((0, 0)));
        assert!(w.get_chunk(0, 0).is_some());
        let pre_edit = collect_live(&w);
        w.set_cell(10, 10, true);
        let post_edit = collect_live(&w);
        assert_eq!(post_edit.len(), pre_edit.len() + 1);
    }

    #[test]
    fn remove_chunk_drops_oscillator_entry() {
        let mut w = World::new();
        place_blinker(&mut w, 5, 5);
        let mut det = crate::oscillator::Detector::new();
        run_until_promoted(&mut w, &mut det, (0, 0), 200);
        assert!(w.is_oscillating((0, 0)));
        assert!(w.remove_chunk((0, 0)));
        assert!(!w.is_oscillating((0, 0)));
        assert!(w.get_chunk(0, 0).is_none());
    }
}
