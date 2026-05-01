//! Browser bindings. Built only with `--features wasm`.
//!
//! Edge-bundle wire format (33 bytes):
//!   [0..8]   top row    [8..16]  bottom row
//!   [16..24] left col   [24..32] right col   (bit y = cell at that col, row y)
//!   [32]     corners    bit0=TL  bit1=TR  bit2=BL  bit3=BR

use crate::{Chunk, EdgeBundle, FrozenMask, StepResult, CHUNK_SIZE};
use std::sync::Arc;
use wasm_bindgen::prelude::*;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn bytes_to_rows(bits: &[u8]) -> [u64; CHUNK_SIZE] {
    assert_eq!(bits.len(), CHUNK_SIZE * 8);
    let mut rows = [0u64; CHUNK_SIZE];
    for (y, row) in rows.iter_mut().enumerate() {
        let b: [u8; 8] = bits[y * 8..y * 8 + 8].try_into().unwrap();
        *row = u64::from_le_bytes(b);
    }
    rows
}

fn rows_to_bytes(rows: &[u64; CHUNK_SIZE]) -> Vec<u8> {
    let mut out = vec![0u8; CHUNK_SIZE * 8];
    for (y, row) in rows.iter().enumerate() {
        out[y * 8..y * 8 + 8].copy_from_slice(&row.to_le_bytes());
    }
    out
}

fn edge_to_bytes(e: &EdgeBundle) -> Vec<u8> {
    let mut out = vec![0u8; 33];
    out[0..8].copy_from_slice(&e.top.to_le_bytes());
    out[8..16].copy_from_slice(&e.bottom.to_le_bytes());
    out[16..24].copy_from_slice(&e.left.to_le_bytes());
    out[24..32].copy_from_slice(&e.right.to_le_bytes());
    out[32] = e.corners[0] | (e.corners[1] << 1) | (e.corners[2] << 2) | (e.corners[3] << 3);
    out
}

fn edge_from_bytes(halo: &[u8]) -> EdgeBundle {
    assert_eq!(halo.len(), 33);
    let top    = u64::from_le_bytes(halo[0..8].try_into().unwrap());
    let bottom = u64::from_le_bytes(halo[8..16].try_into().unwrap());
    let left   = u64::from_le_bytes(halo[16..24].try_into().unwrap());
    let right  = u64::from_le_bytes(halo[24..32].try_into().unwrap());
    let c = halo[32];
    EdgeBundle { top, bottom, left, right,
        corners: [c & 1, (c >> 1) & 1, (c >> 2) & 1, (c >> 3) & 1] }
}

fn build_frozen(frozen_mask: &[u8], rows: &[u64; CHUNK_SIZE]) -> Option<Arc<FrozenMask>> {
    if frozen_mask.len() != CHUNK_SIZE * 8 { return None; }
    let mask  = bytes_to_rows(frozen_mask);
    let mut value = [0u64; CHUNK_SIZE];
    for y in 0..CHUNK_SIZE { value[y] = rows[y] & mask[y]; }
    Some(Arc::new(FrozenMask { mask, value }))
}

// ---------------------------------------------------------------------------
// SIMD128 kernel (2 rows per iteration, mirrors the AVX2 lane structure)
// ---------------------------------------------------------------------------

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
unsafe fn kernel_simd128(
    rows: &[u64; CHUNK_SIZE],
    halo: &EdgeBundle,
    out_rows: &mut [u64; CHUNK_SIZE],
) {
    use std::arch::wasm32::*;

    // Padded row buffer: halo.top | rows[0..64] | halo.bottom
    let mut row_buf = [0u64; CHUNK_SIZE + 2];
    row_buf[0] = halo.top;
    row_buf[1..=CHUNK_SIZE].copy_from_slice(rows);
    row_buf[CHUNK_SIZE + 1] = halo.bottom;

    // Padded boundary-bit buffers: one u64 per padded row, value is 0 or 1.
    let mut left_buf  = [0u64; CHUNK_SIZE + 2];
    let mut right_buf = [0u64; CHUNK_SIZE + 2];
    left_buf[0]  = u64::from(halo.corners[0] & 1);
    right_buf[0] = u64::from(halo.corners[1] & 1);
    for y in 0..CHUNK_SIZE {
        left_buf[y + 1]  = (halo.left  >> y) & 1;
        right_buf[y + 1] = (halo.right >> y) & 1;
    }
    left_buf[CHUNK_SIZE + 1]  = u64::from(halo.corners[2] & 1);
    right_buf[CHUNK_SIZE + 1] = u64::from(halo.corners[3] & 1);

    // Each v128 holds 2 × u64. y iterates over pairs of output rows.
    let mut y = 0usize;
    while y < CHUNK_SIZE {
        let off = y * 8; // byte offset into the padded buffers for this pair

        let rp  = row_buf.as_ptr()   as *const u8;
        let lp  = left_buf.as_ptr()  as *const u8;
        let rtp = right_buf.as_ptr() as *const u8;

        // Load top/mid/bot each as [row_pair_a, row_pair_b].
        let top = v128_load(rp.add(off)      as *const v128);
        let mid = v128_load(rp.add(off +  8) as *const v128);
        let bot = v128_load(rp.add(off + 16) as *const v128);

        let l_top = v128_load(lp.add(off)      as *const v128);
        let l_mid = v128_load(lp.add(off +  8) as *const v128);
        let l_bot = v128_load(lp.add(off + 16) as *const v128);

        // Right boundary bits pre-shifted to bit 63.
        let r_top = i64x2_shl(v128_load(rtp.add(off)      as *const v128), 63);
        let r_mid = i64x2_shl(v128_load(rtp.add(off +  8) as *const v128), 63);
        let r_bot = i64x2_shl(v128_load(rtp.add(off + 16) as *const v128), 63);

        // 8 shifted neighbor patterns (same as scalar, parallel over 2 rows).
        let top_l = v128_or(i64x2_shl(top, 1), l_top);
        let top_r = v128_or(u64x2_shr(top, 1), r_top);
        let mid_l = v128_or(i64x2_shl(mid, 1), l_mid);
        let mid_r = v128_or(u64x2_shr(mid, 1), r_mid);
        let bot_l = v128_or(i64x2_shl(bot, 1), l_bot);
        let bot_r = v128_or(u64x2_shr(bot, 1), r_bot);

        // Bit-parallel 4-bit half-adder across 8 neighbors.
        let mut s0 = i64x2_splat(0);
        let mut s1 = i64x2_splat(0);
        let mut s2 = i64x2_splat(0);
        let mut s3 = i64x2_splat(0);
        for n in [top_l, top, top_r, mid_l, mid_r, bot_l, bot, bot_r] {
            let c0 = v128_and(s0, n); s0 = v128_xor(s0, n);
            let c1 = v128_and(s1, c0); s1 = v128_xor(s1, c0);
            let c2 = v128_and(s2, c1); s2 = v128_xor(s2, c1);
            s3 = v128_or(s3, c2);
        }

        // Conway B3/S23: !s3 & !s2 & s1 & (s0 | mid)
        // v128_andnot(a, b) = a & !b
        let lhs  = v128_and(s1, v128_or(s0, mid));
        let next = v128_andnot(v128_andnot(lhs, s2), s3);

        v128_store(out_rows.as_mut_ptr().add(y) as *mut v128, next);
        y += 2;
    }
}

// ---------------------------------------------------------------------------
// Scalar fallback (delegates to the existing Chunk::step path)
// ---------------------------------------------------------------------------

fn step_scalar(bits: &[u8], halo: &[u8], frozen_mask: &[u8]) -> Vec<u8> {
    let rows = bytes_to_rows(bits);
    let frozen = build_frozen(frozen_mask, &rows);
    let chunk = Chunk::from_rows_and_frozen(rows, frozen);
    match chunk.step(&edge_from_bytes(halo)) {
        StepResult::Stepped(c) => rows_to_bytes(c.rows()),
        StepResult::Unchanged  => bits.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Exported chunk-level API
// ---------------------------------------------------------------------------

/// Compute the edge bundle for a chunk.
/// Input: 512-byte chunk bitset. Output: 33 bytes in edge-bundle wire format.
#[wasm_bindgen]
pub fn chunk_edges(bits: &[u8]) -> Vec<u8> {
    let chunk = Chunk::from_rows_and_frozen(bytes_to_rows(bits), None);
    edge_to_bytes(&chunk.edges())
}

/// Advance a chunk by one GoL step.
/// `bits`: 512-byte chunk bitset.
/// `halo`: 33-byte edge-bundle assembled by the caller from neighbor chunks.
/// `frozen_mask`: 512-byte frozen-cell mask (1 bit = frozen), or empty if none.
/// Returns 512 new bytes.
#[wasm_bindgen]
pub fn step_chunk(bits: &[u8], halo: &[u8], frozen_mask: &[u8]) -> Vec<u8> {
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    {
        let rows = bytes_to_rows(bits);
        let halo_bundle = edge_from_bytes(halo);
        if rows.iter().all(|&r| r == 0) && halo_bundle.is_zero() {
            return bits.to_vec();
        }
        let mut out = [0u64; CHUNK_SIZE];
        unsafe { kernel_simd128(&rows, &halo_bundle, &mut out); }
        if frozen_mask.len() == CHUNK_SIZE * 8 {
            let mask = bytes_to_rows(frozen_mask);
            for y in 0..CHUNK_SIZE {
                out[y] = (out[y] & !mask[y]) | (rows[y] & mask[y]);
            }
        }
        return rows_to_bytes(&out);
    }
    #[allow(unreachable_code)]
    step_scalar(bits, halo, frozen_mask)
}

// ---------------------------------------------------------------------------
// Full-world API (retained for completeness)
// ---------------------------------------------------------------------------

#[wasm_bindgen]
pub struct WasmWorld(crate::World);

#[wasm_bindgen]
impl WasmWorld {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self { Self(crate::World::new()) }
    pub fn tick(&mut self) { self.0.tick(); }
    pub fn tick_number(&self) -> u64 { self.0.tick_number() }
    pub fn set_cell(&mut self, x: i64, y: i64, alive: bool) { self.0.set_cell(x, y, alive); }
    pub fn live_count(&self) -> u32 { self.0.iter_chunks().map(|(_, c)| c.live_count()).sum() }
}

impl Default for WasmWorld {
    fn default() -> Self { Self::new() }
}
