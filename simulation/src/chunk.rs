//! `CHUNK_SIZE x CHUNK_SIZE` packed-bitset chunk + bit-parallel step.

use crate::CHUNK_SIZE;
use std::sync::Arc;

#[cfg(all(feature = "avx2", not(target_arch = "x86_64")))]
compile_error!("feature `avx2` requires target_arch = \"x86_64\"");

const LAST_BIT: usize = CHUNK_SIZE - 1;
const ROW_MASK: u64 = if CHUNK_SIZE == 64 {
    u64::MAX
} else {
    (1u64 << CHUNK_SIZE) - 1
};
const ROW_BYTES: usize = std::mem::size_of::<u64>();
const AVX2_LANE_BYTES: usize = 32;
const AVX2_LANE_ROWS: usize = AVX2_LANE_BYTES / ROW_BYTES;
const AVX2_LANES_PER_CHUNK: usize = (CHUNK_SIZE * ROW_BYTES) / AVX2_LANE_BYTES;
const _: () = assert!(
    CHUNK_SIZE % AVX2_LANE_ROWS == 0,
    "AVX2 kernels process whole 4-row lanes",
);
const _: () = assert!(CHUNK_SIZE <= 64, "row stored as u64");

const HASH_MIX_K: u64 = 0x9E37_79B9_7F4A_7C15;

/// One row per `u64`; bit `x` of `rows[y]` = cell at `(x, y)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    rows: [u64; CHUNK_SIZE],
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

    pub fn from_rows_and_frozen(
        rows: [u64; CHUNK_SIZE],
        frozen: Option<Arc<FrozenMask>>,
    ) -> Self {
        Self { rows, frozen }
    }

    pub fn rows(&self) -> &[u64; CHUNK_SIZE] {
        &self.rows
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

    /// 64-bit multiply-mix over the row bits. Non-cryptographic; intended for
    /// cycle-detection identity checks where two equal `rows` must hash equal.
    pub fn hash_state(&self) -> u64 {
        let mut h: u64 = HASH_MIX_K;
        for &r in &self.rows {
            h ^= r;
            h = h.wrapping_mul(HASH_MIX_K);
            h ^= h >> (u64::BITS / 2);
        }
        h
    }

    pub fn is_empty(&self) -> bool {
        #[cfg(feature = "avx2")]
        // SAFETY: `avx2` feature is a build-time promise of AVX2 support.
        unsafe { return is_empty_avx2(&self.rows); }
        #[cfg(not(feature = "avx2"))]
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
    pub fn step(&self, halo: &EdgeBundle) -> StepResult {
        // Empty + zero halo: result is provably identical to input. One branch
        // instead of ~3000 kernel ops, and no clone.
        if self.is_empty() && halo.is_zero() {
            return StepResult::Unchanged;
        }
        let mut out_rows = [0u64; CHUNK_SIZE];
        #[cfg(feature = "avx2")]
        // SAFETY: `avx2` feature is a build-time promise of AVX2 support.
        unsafe { kernel_avx2(&self.rows, halo, &mut out_rows) };
        #[cfg(not(feature = "avx2"))]
        kernel_scalar(&self.rows, halo, &mut out_rows);

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
        StepResult::Stepped(out)
    }
}

#[cfg_attr(feature = "avx2", allow(dead_code))]
fn kernel_scalar(rows: &[u64; CHUNK_SIZE], halo: &EdgeBundle, out_rows: &mut [u64; CHUNK_SIZE]) {
    let row_at = |y: i32| -> u64 {
        if y < 0 {
            halo.top
        } else if y as usize >= CHUNK_SIZE {
            halo.bottom
        } else {
            rows[y as usize]
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

    for y in 0..CHUNK_SIZE {
        let yi = y as i32;
        let mid = rows[y];
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
}

#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(any(feature = "avx2", test)), allow(dead_code))]
#[target_feature(enable = "avx2")]
unsafe fn is_empty_avx2(rows: &[u64; CHUNK_SIZE]) -> bool {
    use std::arch::x86_64::*;
    let p = rows.as_ptr().cast::<__m256i>();
    let mut acc = _mm256_loadu_si256(p);
    let mut i = 1usize;
    while i < AVX2_LANES_PER_CHUNK {
        acc = _mm256_or_si256(acc, _mm256_loadu_si256(p.byte_add(i * AVX2_LANE_BYTES)));
        i += 1;
    }
    _mm256_testz_si256(acc, acc) != 0
}

#[cfg_attr(not(any(feature = "avx2", test)), allow(dead_code))]
#[target_feature(enable = "avx2")]
unsafe fn kernel_avx2(
    rows: &[u64; CHUNK_SIZE],
    halo: &EdgeBundle,
    out_rows: &mut [u64; CHUNK_SIZE],
) {
    use std::arch::x86_64::*;

    // Padded so unaligned 256-bit loads at offsets 0/1/2 cover top/mid/bot rows.
    let mut row_buf = [0u64; CHUNK_SIZE + 2];
    row_buf[0] = halo.top;
    row_buf[1..=CHUNK_SIZE].copy_from_slice(rows);
    row_buf[CHUNK_SIZE + 1] = halo.bottom;

    let mut left_buf = [0u64; CHUNK_SIZE + 2];
    let mut right_buf = [0u64; CHUNK_SIZE + 2];
    left_buf[0] = u64::from(halo.corners[0] & 1);
    right_buf[0] = u64::from(halo.corners[1] & 1);
    for y in 0..CHUNK_SIZE {
        left_buf[y + 1] = (halo.left >> y) & 1;
        right_buf[y + 1] = (halo.right >> y) & 1;
    }
    left_buf[CHUNK_SIZE + 1] = u64::from(halo.corners[2] & 1);
    right_buf[CHUNK_SIZE + 1] = u64::from(halo.corners[3] & 1);

    let row_ptr = row_buf.as_ptr().cast::<__m256i>();
    let left_ptr = left_buf.as_ptr().cast::<__m256i>();
    let right_ptr = right_buf.as_ptr().cast::<__m256i>();

    let mut y = 0usize;
    while y < CHUNK_SIZE {
        let top = _mm256_loadu_si256(row_ptr.byte_add(y * ROW_BYTES));
        let mid = _mm256_loadu_si256(row_ptr.byte_add((y + 1) * ROW_BYTES));
        let bot = _mm256_loadu_si256(row_ptr.byte_add((y + 2) * ROW_BYTES));

        let l_top = _mm256_loadu_si256(left_ptr.byte_add(y * ROW_BYTES));
        let l_mid = _mm256_loadu_si256(left_ptr.byte_add((y + 1) * ROW_BYTES));
        let l_bot = _mm256_loadu_si256(left_ptr.byte_add((y + 2) * ROW_BYTES));

        let r_top = _mm256_slli_epi64(_mm256_loadu_si256(right_ptr.byte_add(y * ROW_BYTES)), LAST_BIT as i32);
        let r_mid = _mm256_slli_epi64(_mm256_loadu_si256(right_ptr.byte_add((y + 1) * ROW_BYTES)), LAST_BIT as i32);
        let r_bot = _mm256_slli_epi64(_mm256_loadu_si256(right_ptr.byte_add((y + 2) * ROW_BYTES)), LAST_BIT as i32);

        let top_l = _mm256_or_si256(_mm256_slli_epi64(top, 1), l_top);
        let top_r = _mm256_or_si256(_mm256_srli_epi64(top, 1), r_top);
        let mid_l = _mm256_or_si256(_mm256_slli_epi64(mid, 1), l_mid);
        let mid_r = _mm256_or_si256(_mm256_srli_epi64(mid, 1), r_mid);
        let bot_l = _mm256_or_si256(_mm256_slli_epi64(bot, 1), l_bot);
        let bot_r = _mm256_or_si256(_mm256_srli_epi64(bot, 1), r_bot);

        let neighbors = [top_l, top, top_r, mid_l, mid_r, bot_l, bot, bot_r];

        let mut s0 = _mm256_setzero_si256();
        let mut s1 = _mm256_setzero_si256();
        let mut s2 = _mm256_setzero_si256();
        let mut s3 = _mm256_setzero_si256();
        for n in neighbors {
            let c0 = _mm256_and_si256(s0, n);
            s0 = _mm256_xor_si256(s0, n);
            let c1 = _mm256_and_si256(s1, c0);
            s1 = _mm256_xor_si256(s1, c0);
            let c2 = _mm256_and_si256(s2, c1);
            s2 = _mm256_xor_si256(s2, c1);
            s3 = _mm256_or_si256(s3, c2);
        }
        let s0_or_mid = _mm256_or_si256(s0, mid);
        let lhs = _mm256_and_si256(s1, s0_or_mid);
        let next = _mm256_andnot_si256(s3, _mm256_andnot_si256(s2, lhs));

        let out_ptr = out_rows.as_mut_ptr().byte_add(y * ROW_BYTES).cast::<__m256i>();
        _mm256_storeu_si256(out_ptr, next);
        y += AVX2_LANE_ROWS;
    }
}

/// Outcome of [`Chunk::step`]. `Unchanged` only fires on the empty + zero-halo
/// early-out so callers can skip insert without paying for a clone.
// `Stepped` is large by design (inline `[u64; 64]` rows): boxing would add a
// heap alloc per stepped chunk, defeating the whole point of the Unchanged path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepResult {
    Unchanged,
    Stepped(Chunk),
}

impl StepResult {
    #[cfg(test)]
    fn into_chunk(self, source: &Chunk) -> Chunk {
        match self {
            StepResult::Unchanged => source.clone(),
            StepResult::Stepped(c) => c,
        }
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
        let mut rows = [0u64; CHUNK_SIZE];
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
                if matches!((alive, count), (true, 2 | 3) | (false, 3)) {
                    rows[y] |= 1u64 << x;
                }
            }
        }
        let frozen = chunk.frozen.clone();
        if let Some(m) = frozen.as_ref() {
            for y in 0..CHUNK_SIZE {
                rows[y] = (rows[y] & !m.mask[y]) | (m.value[y] & m.mask[y]);
            }
        }
        Chunk::from_rows_and_frozen(rows, frozen)
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
            let mut rows = [0u64; CHUNK_SIZE];
            for r in &mut rows {
                *r = rng.next() & ROW_MASK;
            }
            let chunk = Chunk::from_rows_and_frozen(rows, None);
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
            let actual = chunk.step(&halo).into_chunk(&chunk);
            assert_eq!(actual, reference_step(&chunk, &halo));
        }
    }

    #[test]
    fn blinker_oscillates() {
        let mut c = Chunk::empty();
        c.set(1, 1, true);
        c.set(2, 1, true);
        c.set(3, 1, true);
        let halo = EdgeBundle::empty();
        let next = c.step(&halo).into_chunk(&c);
        assert!(next.get(2, 0) && next.get(2, 1) && next.get(2, 2));
        assert_eq!(next.live_count(), 3);
        assert_eq!(next.step(&halo).into_chunk(&next), c);
    }

    #[test]
    fn block_is_still() {
        let mut c = Chunk::empty();
        c.set(1, 1, true);
        c.set(2, 1, true);
        c.set(1, 2, true);
        c.set(2, 2, true);
        assert_eq!(c.step(&EdgeBundle::empty()).into_chunk(&c), c);
    }

    #[test]
    fn empty_stays_empty() {
        assert_eq!(
            Chunk::empty().step(&EdgeBundle::empty()),
            StepResult::Unchanged
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_is_empty_matches_scalar() {
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }
        let mut rng = Xs(0xfeed_face_dead_beef);
        for _ in 0..64 {
            let mut rows = [0u64; CHUNK_SIZE];
            if rng.next() & 1 == 1 {
                let idx = (rng.next() as usize) % CHUNK_SIZE;
                let bit = rng.next() & ROW_MASK;
                if bit != 0 { rows[idx] = bit; }
            }
            let scalar = rows.iter().all(|r| *r == 0);
            let avx2 = unsafe { is_empty_avx2(&rows) };
            assert_eq!(scalar, avx2, "is_empty disagrees on rows {:?}", rows);
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_matches_scalar_random() {
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }
        let mut rng = Xs(0x1234_5678_9abc_def0);
        for _ in 0..64 {
            let mut rows = [0u64; CHUNK_SIZE];
            for r in &mut rows {
                *r = rng.next() & ROW_MASK;
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
            let mut scalar_out = [0u64; CHUNK_SIZE];
            let mut avx2_out = [0u64; CHUNK_SIZE];
            kernel_scalar(&rows, &halo, &mut scalar_out);
            unsafe { kernel_avx2(&rows, &halo, &mut avx2_out) };
            assert_eq!(scalar_out, avx2_out);
        }
    }

    #[test]
    fn frozen_cells_persist() {
        let mut c = Chunk::empty();
        c.freeze(5, 5, true);
        let next = c.step(&EdgeBundle::empty()).into_chunk(&c);
        assert!(next.get(5, 5));
        assert!(next.is_frozen());
    }
}
