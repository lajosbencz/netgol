//! `CHUNK_SIZE x CHUNK_SIZE` packed-bitset chunk + bit-parallel step.

use crate::CHUNK_SIZE;
use std::sync::Arc;

const LAST_BIT: usize = CHUNK_SIZE - 1;
const ROW_MASK: u64 = if CHUNK_SIZE == 64 {
    u64::MAX
} else {
    (1u64 << CHUNK_SIZE) - 1
};

/// One row per `u64`; bit `x` of `rows[y]` = cell at `(x, y)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub rows: [u64; CHUNK_SIZE],
    pub frozen: Option<Arc<FrozenMask>>,
}

impl Default for Chunk {
    fn default() -> Self {
        Self::empty()
    }
}

impl Chunk {
    pub const fn empty() -> Self {
        Self {
            rows: [0u64; CHUNK_SIZE],
            frozen: None,
        }
    }

    pub fn get(&self, x: usize, y: usize) -> bool {
        assert!(x < CHUNK_SIZE && y < CHUNK_SIZE);
        (self.rows[y] >> x) & 1 == 1
    }

    pub fn set(&mut self, x: usize, y: usize, alive: bool) {
        assert!(x < CHUNK_SIZE && y < CHUNK_SIZE);
        if let Some(m) = &self.frozen {
            if (m.mask[y] >> x) & 1 == 1 {
                return; // frozen - silently ignore
            }
        }
        let bit = 1u64 << x;
        if alive {
            self.rows[y] |= bit;
        } else {
            self.rows[y] &= !bit;
        }
    }

    pub fn live_count(&self) -> u32 {
        self.rows.iter().map(|r| r.count_ones()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.iter().all(|r| *r == 0)
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.is_some()
    }

    /// Lock cell `(x, y)` at value `alive`; future steps cannot change it.
    pub fn freeze(&mut self, x: usize, y: usize, alive: bool) {
        assert!(x < CHUNK_SIZE && y < CHUNK_SIZE);
        let bit = 1u64 << x;
        let arc = self.frozen.get_or_insert_with(|| Arc::new(FrozenMask::empty()));
        let mask = Arc::make_mut(arc);
        mask.mask[y] |= bit;
        if alive {
            mask.value[y] |= bit;
        } else {
            mask.value[y] &= !bit;
        }
        // Apply immediately so reads/edges reflect the locked value.
        if alive {
            self.rows[y] |= bit;
        } else {
            self.rows[y] &= !bit;
        }
    }

    pub fn unfreeze(&mut self, x: usize, y: usize) {
        assert!(x < CHUNK_SIZE && y < CHUNK_SIZE);
        if let Some(arc) = self.frozen.as_mut() {
            let mask = Arc::make_mut(arc);
            let bit = !(1u64 << x);
            mask.mask[y] &= bit;
            mask.value[y] &= bit;
            if mask.mask.iter().all(|r| *r == 0) {
                self.frozen = None;
            }
        }
    }

    /// Top/bottom rows + left/right columns + 4 corners, packed for halo assembly.
    pub fn edges(&self) -> EdgeBundle {
        let top = self.rows[0];
        let bottom = self.rows[LAST_BIT];
        let mut left = 0u64;
        let mut right = 0u64;
        for y in 0..CHUNK_SIZE {
            left |= (self.rows[y] & 1) << y;
            right |= ((self.rows[y] >> LAST_BIT) & 1) << y;
        }
        EdgeBundle {
            top,
            bottom,
            left,
            right,
            corners: [
                (top & 1) as u8,
                ((top >> LAST_BIT) & 1) as u8,
                (bottom & 1) as u8,
                ((bottom >> LAST_BIT) & 1) as u8,
            ],
        }
    }

    /// One GoL step against the supplied halo. Frozen cells are re-applied at the end.
    ///
    /// Bit-parallel half-adder cascade across the 8 shifted neighbor rows. Per column
    /// `next = !s3 & !s2 & s1 & (s0 | alive)` - the standard B3/S23 rule reduced from
    /// the 4-bit neighbor count `(s3 s2 s1 s0)`.
    pub fn step(&self, halo: &EdgeBundle) -> Self {
        // Early-out: empty chunk + zero halo => empty result. The bit-parallel
        // kernel would compute zero-of-everything for ~3000 ops; this is one branch.
        // Frozen masks are preserved through the clone.
        if self.is_empty() && halo.is_zero() {
            return self.clone();
        }
        let row_at = |y: i32| -> u64 {
            if y < 0 {
                halo.top
            } else if y as usize >= CHUNK_SIZE {
                halo.bottom
            } else {
                self.rows[y as usize]
            }
        };
        let left_bit = |y: i32| -> u64 {
            if y < 0 {
                u64::from(halo.corners[0] & 1)
            } else if y as usize >= CHUNK_SIZE {
                u64::from(halo.corners[2] & 1)
            } else {
                (halo.left >> y) & 1
            }
        };
        let right_bit = |y: i32| -> u64 {
            if y < 0 {
                u64::from(halo.corners[1] & 1)
            } else if y as usize >= CHUNK_SIZE {
                u64::from(halo.corners[3] & 1)
            } else {
                (halo.right >> y) & 1
            }
        };

        let mut out_rows = [0u64; CHUNK_SIZE];
        for y in 0..CHUNK_SIZE {
            let yi = y as i32;
            let mid = self.rows[y];
            let top = row_at(yi - 1);
            let bot = row_at(yi + 1);

            let top_l = (top << 1) | left_bit(yi - 1);
            let top_r = (top >> 1) | (right_bit(yi - 1) << LAST_BIT);
            let mid_l = (mid << 1) | left_bit(yi);
            let mid_r = (mid >> 1) | (right_bit(yi) << LAST_BIT);
            let bot_l = (bot << 1) | left_bit(yi + 1);
            let bot_r = (bot >> 1) | (right_bit(yi + 1) << LAST_BIT);

            let neighbors = [top_l, top, top_r, mid_l, mid_r, bot_l, bot, bot_r];

            let (mut s0, mut s1, mut s2, mut s3) = (0u64, 0u64, 0u64, 0u64);
            for n in neighbors {
                let c0 = s0 & n;
                s0 ^= n;
                let c1 = s1 & c0;
                s1 ^= c0;
                let c2 = s2 & c1;
                s2 ^= c1;
                s3 |= c2;
            }
            out_rows[y] = (!s3 & !s2 & s1 & (s0 | mid)) & ROW_MASK;
        }

        let mut out = Chunk {
            rows: out_rows,
            frozen: self.frozen.clone(),
        };
        if let Some(mask) = out.frozen.as_ref() {
            for y in 0..CHUNK_SIZE {
                let m = mask.mask[y];
                let v = mask.value[y];
                out.rows[y] = (out.rows[y] & !m) | (v & m);
            }
        }
        out
    }
}

/// Halo around a chunk under step: 4 edges (each as one `u64`) + 4 corner bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EdgeBundle {
    pub top: u64,
    pub bottom: u64,
    pub left: u64,
    pub right: u64,
    /// TL, TR, BL, BR
    pub corners: [u8; 4],
}

impl EdgeBundle {
    pub const fn empty() -> Self {
        Self {
            top: 0,
            bottom: 0,
            left: 0,
            right: 0,
            corners: [0u8; 4],
        }
    }

    pub const fn is_zero(&self) -> bool {
        self.top == 0 && self.bottom == 0 && self.left == 0 && self.right == 0
            && self.corners[0] == 0 && self.corners[1] == 0
            && self.corners[2] == 0 && self.corners[3] == 0
    }
}

/// Per-chunk frozen-cell mask. `mask` bits = "frozen here", `value` bits = pinned alive/dead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenMask {
    pub mask: [u64; CHUNK_SIZE],
    pub value: [u64; CHUNK_SIZE],
}

impl FrozenMask {
    pub const fn empty() -> Self {
        Self {
            mask: [0u64; CHUNK_SIZE],
            value: [0u64; CHUNK_SIZE],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_step(chunk: &Chunk, halo: &EdgeBundle) -> Chunk {
        let n = CHUNK_SIZE as i32;
        let neighbor = |x: i32, y: i32| -> bool {
            if x < 0 && y < 0 {
                halo.corners[0] != 0
            } else if x >= n && y < 0 {
                halo.corners[1] != 0
            } else if x < 0 && y >= n {
                halo.corners[2] != 0
            } else if x >= n && y >= n {
                halo.corners[3] != 0
            } else if y < 0 {
                (halo.top >> x) & 1 == 1
            } else if y >= n {
                (halo.bottom >> x) & 1 == 1
            } else if x < 0 {
                (halo.left >> y) & 1 == 1
            } else if x >= n {
                (halo.right >> y) & 1 == 1
            } else {
                chunk.get(x as usize, y as usize)
            }
        };
        let mut out = Chunk::empty();
        for y in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let mut count = 0u8;
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        if neighbor(x as i32 + dx, y as i32 + dy) {
                            count += 1;
                        }
                    }
                }
                let alive = chunk.get(x, y);
                out.set(x, y, matches!((alive, count), (true, 2 | 3) | (false, 3)));
            }
        }
        out.frozen = chunk.frozen.clone();
        if let Some(m) = out.frozen.as_ref() {
            for y in 0..CHUNK_SIZE {
                out.rows[y] = (out.rows[y] & !m.mask[y]) | (m.value[y] & m.mask[y]);
            }
        }
        out
    }

    struct Xs(u64);
    impl Xs {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    #[test]
    fn bit_parallel_matches_reference_random() {
        let mut rng = Xs(0x00de_adbe_efc0_ffee);
        for _ in 0..64 {
            let mut chunk = Chunk::empty();
            for y in 0..CHUNK_SIZE {
                chunk.rows[y] = rng.next() & ROW_MASK;
            }
            let halo = EdgeBundle {
                top: rng.next() & ROW_MASK,
                bottom: rng.next() & ROW_MASK,
                left: rng.next() & ROW_MASK,
                right: rng.next() & ROW_MASK,
                corners: [
                    (rng.next() & 1) as u8,
                    (rng.next() & 1) as u8,
                    (rng.next() & 1) as u8,
                    (rng.next() & 1) as u8,
                ],
            };
            assert_eq!(chunk.step(&halo), reference_step(&chunk, &halo));
        }
    }

    #[test]
    fn blinker_oscillates() {
        let mut c = Chunk::empty();
        c.set(1, 1, true);
        c.set(2, 1, true);
        c.set(3, 1, true);
        let halo = EdgeBundle::empty();
        let next = c.step(&halo);
        assert!(next.get(2, 0) && next.get(2, 1) && next.get(2, 2));
        assert_eq!(next.live_count(), 3);
        assert_eq!(next.step(&halo), c);
    }

    #[test]
    fn block_is_still() {
        let mut c = Chunk::empty();
        c.set(1, 1, true);
        c.set(2, 1, true);
        c.set(1, 2, true);
        c.set(2, 2, true);
        assert_eq!(c.step(&EdgeBundle::empty()), c);
    }

    #[test]
    fn empty_stays_empty() {
        assert!(Chunk::empty().step(&EdgeBundle::empty()).is_empty());
    }

    #[test]
    fn frozen_cells_persist() {
        let mut c = Chunk::empty();
        c.freeze(5, 5, true);
        // No live neighbors; without freeze, (5,5) would die. With freeze, it survives.
        let next = c.step(&EdgeBundle::empty());
        assert!(next.get(5, 5));
        assert!(next.is_frozen());
    }
}
