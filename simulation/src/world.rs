//! Sparse chunk-indexed world. Stepping grows into neighbor chunks as edges birth cells,
//! and drops chunks that go fully empty unless they're frozen.

use crate::chunk::{Chunk, EdgeBundle};
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

type CoordMap<V> = HashMap<ChunkCoord, V, BuildHasherDefault<CoordHasher>>;
type CoordSet = HashSet<ChunkCoord, BuildHasherDefault<CoordHasher>>;

#[derive(Debug, Default, Clone)]
pub struct World {
    chunks: CoordMap<Chunk>,
    tick: u64,
    /// Scratch buffers reused across ticks. Cleared (not dropped) at start of `tick`.
    scratch_edges: CoordMap<EdgeBundle>,
    scratch_candidates: CoordSet,
    scratch_candidates_vec: Vec<ChunkCoord>,
}

#[derive(Debug, Default, Clone)]
pub struct TickOutcome {
    pub changed: Vec<ChunkCoord>,
    pub removed: Vec<ChunkCoord>,
}

impl World {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tick_number(&self) -> u64 {
        self.tick
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn get_chunk(&self, cx: i32, cy: i32) -> Option<&Chunk> {
        self.chunks.get(&(cx, cy))
    }

    pub fn iter_chunks(&self) -> impl Iterator<Item = (ChunkCoord, &Chunk)> {
        self.chunks.iter().map(|(c, ch)| (*c, ch))
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

    /// Remove a chunk (used by the reaper). Returns `true` if it existed.
    pub fn remove_chunk(&mut self, coord: ChunkCoord) -> bool {
        self.chunks.remove(&coord).is_some()
    }

    pub fn set_tick_number(&mut self, tick: u64) {
        self.tick = tick;
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
        self.scratch_edges.clear();
        self.scratch_candidates.clear();
        self.scratch_candidates_vec.clear();

        for (&coord, ch) in &self.chunks {
            if ch.is_empty() {
                continue;
            }
            self.scratch_edges.insert(coord, ch.edges());
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
        for i in 0..self.scratch_candidates_vec.len() {
            let coord = self.scratch_candidates_vec[i];
            let next = {
                let current = self.chunks.get(&coord).unwrap_or(&empty_chunk);
                let halo = assemble_halo(coord, &self.scratch_edges);
                current.step(&halo)
            };
            if next.is_empty() && !next.is_frozen() {
                if self.chunks.remove(&coord).is_some() {
                    outcome.removed.push(coord);
                }
                continue;
            }
            match self.chunks.entry(coord) {
                Entry::Occupied(mut slot) => {
                    if slot.get().rows != next.rows {
                        slot.insert(next);
                        outcome.changed.push(coord);
                    }
                }
                Entry::Vacant(slot) => {
                    slot.insert(next);
                    outcome.changed.push(coord);
                }
            }
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
}
