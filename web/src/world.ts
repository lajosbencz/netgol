// Chunk cache. Keyed by "cx,cy". Lifetime is independent of subscription set -
// only `Reaped`, LRU eviction, and disconnect drop entries. Renderer reads from this
// cache; staleness across a pan is preferred over flicker.
//
// Per-chunk frozen masks are materialised locally from the region table the
// server sends in `Regions`. They're not on the per-chunk wire frame.

import { CHUNK_SIZE, BITS_BYTES, FLAG_FROZEN, Region, EditCell } from './protocol';
export type ChunkKey = string;
export const key = (cx: number, cy: number): ChunkKey => `${cx},${cy}`;

// ---------------------------------------------------------------------------
// WASM GoL kernel. Edge-bundle wire format (33 bytes):
//   [0..8]   top row    [8..16]  bottom row
//   [16..24] left col   [24..32] right col
//   [32]     corners    bit0=TL  bit1=TR  bit2=BL  bit3=BR
// ---------------------------------------------------------------------------

// Probe: minimal WASM binary using v128.const — validates iff SIMD128 is supported.
const SIMD_SUPPORTED = WebAssembly.validate(new Uint8Array([
  0x00,0x61,0x73,0x6d, 0x01,0x00,0x00,0x00, // magic + version
  0x01,0x05,0x01,0x60,0x00,0x01,0x7b,        // type: () -> v128
  0x03,0x02,0x01,0x00,                        // function section
  0x0a,0x16,0x01,0x14,0x00,                  // code section
  0xfd,0x0c,                                  // v128.const
  0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
  0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
  0x0b,                                       // end
]));

type WasmFn = { chunk_edges: (b: Uint8Array) => Uint8Array; step_chunk: (b: Uint8Array, h: Uint8Array, f: Uint8Array) => Uint8Array };
let wasm: WasmFn;

if (SIMD_SUPPORTED) {
  const m = await import('../wasm/simd/simulation.js');
  await m.default();
  wasm = m;
} else {
  const m = await import('../wasm/scalar/simulation.js');
  await m.default();
  wasm = m;
}

const { chunk_edges, step_chunk } = wasm;

const DEAD_EDGE = new Uint8Array(33);

// Assemble a 33-byte halo from the 8 pre-computed neighbor edge bundles.
// This is pure byte arithmetic; the actual GoL step runs in WASM.
function assembleHalo(
  above: Uint8Array, below: Uint8Array,
  leftN: Uint8Array, rightN: Uint8Array,
  tl: Uint8Array, tr: Uint8Array, bl: Uint8Array, br: Uint8Array,
): Uint8Array {
  const h = new Uint8Array(33);
  h.set(above.subarray(8, 16), 0);    // top    = above.bottom
  h.set(below.subarray(0, 8),  8);    // bottom = below.top
  h.set(leftN.subarray(24, 32), 16);  // left   = leftN.right
  h.set(rightN.subarray(16, 24), 24); // right  = rightN.left
  // TL=BR(tl), TR=BL(tr), BL=TR(bl), BR=TL(br)
  h[32] = ((tl[32] >> 3) & 1)
        | (((tr[32] >> 2) & 1) << 1)
        | (((bl[32] >> 1) & 1) << 2)
        | ((br[32] & 1) << 3);
  return h;
}


export type ChunkEntry = {
  cx: number;
  cy: number;
  tick: bigint;
  /** Raw bitset as received on the wire. Bit `x` of byte `y*8 + (x>>3)` = cell alive. */
  bits: Uint8Array;
  /** Frozen mask (1 bit per cell). `null` if the chunk has no frozen cells. */
  frozenMask: Uint8Array | null;
  /** CHUNK_SIZExCHUNK_SIZE RGBA bitmap, repainted from `bits`. drawImage source. */
  canvas: OffscreenCanvas;
};

export type Palette = {
  alive: [number, number, number, number];
  frozenAlive: [number, number, number, number];
  frozenDead: [number, number, number, number];
};

export class ChunkCache {
  /** Map iteration is insertion-ordered; reinserting on access yields LRU semantics. */
  private map = new Map<ChunkKey, ChunkEntry>();
  /** One reusable ImageData per cache instance; saves a 16 KB alloc per repaint. */
  private scratch: ImageData | null = null;
  /** Most recent region table from the server; used to derive per-chunk frozen masks. */
  private regions: Region[] = [];
  /**
   * Chunks that received a server-authoritative `ChunkDelta` this tick.
   * These are already at the correct post-GoL state; `step()` must skip them
   * to avoid double-advancing the simulation.
   */
  private updatedThisTick = new Set<ChunkKey>();

  constructor(private capacity: number, private palette: Palette) {}

  setRegions(regions: Region[]) {
    this.regions = regions;
    for (const e of this.map.values()) {
      e.frozenMask = frozenMaskForChunk(regions, e.cx, e.cy);
      this.repaint(e);
    }
  }

  setCapacity(n: number) {
    this.capacity = n;
    this.evictIfNeeded();
  }

  get(cx: number, cy: number): ChunkEntry | undefined {
    return this.map.get(key(cx, cy));
  }

  entries(): IterableIterator<ChunkEntry> { return this.map.values(); }

  size(): number { return this.map.size; }

  /**
   * @param authoritative - true for `ChunkDelta` (server already ran GoL this tick;
   *   skip local GoL step and paint immediately). false for `ChunkState` (seed state;
   *   defer first paint until after the first GoL step so boundary cells integrate).
   */
  put(cx: number, cy: number, tick: bigint, bits: Uint8Array, authoritative = false) {
    if (bits.length !== BITS_BYTES) throw new Error('bad bits length');
    const k = key(cx, cy);
    let entry = this.map.get(k);
    if (entry) {
      this.map.delete(k);
      entry.tick = tick;
      entry.bits = bits;
    } else {
      const frozenMask = frozenMaskForChunk(this.regions, cx, cy);
      entry = { cx, cy, tick, bits, frozenMask, canvas: new OffscreenCanvas(CHUNK_SIZE, CHUNK_SIZE) };
    }
    this.repaint(entry);
    this.map.set(k, entry);
    if (authoritative) this.updatedThisTick.add(k);
    this.evictIfNeeded();
  }

  private repaint(e: ChunkEntry) {
    const ctx = e.canvas.getContext('2d')!;
    if (!this.scratch) this.scratch = ctx.createImageData(CHUNK_SIZE, CHUNK_SIZE);
    paintInto(this.scratch, e.bits, e.frozenMask, this.palette);
    ctx.putImageData(this.scratch, 0, 0);
  }

  drop(cx: number, cy: number) {
    const k = key(cx, cy);
    this.map.delete(k);
    this.updatedThisTick.delete(k);
  }

  clear() {
    this.map.clear();
    this.updatedThisTick.clear();
  }

  /** Apply a set of cell edits to a cached chunk without running a GoL step.
   *  Called when `EditApplied` arrives; the GoL step happens on the next `Sync`. */
  applyEdit(cx: number, cy: number, cells: EditCell[]) {
    const entry = this.map.get(key(cx, cy));
    if (!entry) return;
    for (const c of cells) {
      const byteIdx = c.ly * 8 + (c.lx >> 3);
      const mask = 1 << (c.lx & 7);
      if (c.alive) {
        entry.bits[byteIdx] |= mask;
      } else {
        entry.bits[byteIdx] &= ~mask;
      }
    }
    this.repaint(entry);
  }

  /** Advance all cached chunks by one GoL step. Called on `Sync`.
   *  Chunks that received a `ChunkDelta` this tick are already at the correct
   *  post-GoL state and are skipped. Edges are snapshotted from the pre-step
   *  state before any chunk is mutated, matching the server's tick_into approach. */
  step(tick: bigint) {
    const skip = this.updatedThisTick;
    this.updatedThisTick = new Set<ChunkKey>();

    // Snapshot edges (WASM) for all entries before mutating any bits.
    const edgeMap = new Map<ChunkKey, Uint8Array>();
    for (const entry of this.map.values()) {
      edgeMap.set(key(entry.cx, entry.cy), chunk_edges(entry.bits));
    }

    const nb = (dcx: number, dcy: number, cx: number, cy: number): Uint8Array =>
      edgeMap.get(key(cx + dcx, cy + dcy)) ?? DEAD_EDGE;

    for (const entry of this.map.values()) {
      const k = key(entry.cx, entry.cy);
      entry.tick = tick;
      if (skip.has(k)) continue;
      const { cx, cy } = entry;
      const halo = assembleHalo(
        nb(0, -1, cx, cy), nb(0, 1, cx, cy),
        nb(-1, 0, cx, cy), nb(1, 0, cx, cy),
        nb(-1, -1, cx, cy), nb(1, -1, cx, cy),
        nb(-1, 1, cx, cy),  nb(1, 1, cx, cy),
      );
      entry.bits = step_chunk(entry.bits, halo, entry.frozenMask ?? new Uint8Array(0));
      this.repaint(entry);
    }
  }

  private evictIfNeeded() {
    while (this.map.size > this.capacity) {
      const oldest = this.map.keys().next().value;
      if (oldest === undefined) return;
      this.map.delete(oldest);
    }
  }
}

function paintInto(img: ImageData, bits: Uint8Array, mask: Uint8Array | null, p: Palette) {
  const data = img.data;
  for (let y = 0; y < CHUNK_SIZE; y++) {
    const off = y * 8;
    for (let x = 0; x < CHUNK_SIZE; x++) {
      const bit = (bits[off + (x >> 3)] >> (x & 7)) & 1;
      const frozen = mask ? (mask[off + (x >> 3)] >> (x & 7)) & 1 : 0;
      const i = (y * CHUNK_SIZE + x) * 4;
      const c = frozen ? (bit ? p.frozenAlive : p.frozenDead) : (bit ? p.alive : null);
      if (c) {
        data[i] = c[0]; data[i + 1] = c[1]; data[i + 2] = c[2]; data[i + 3] = c[3];
      } else {
        data[i] = 0; data[i + 1] = 0; data[i + 2] = 0; data[i + 3] = 0;
      }
    }
  }
}

function frozenMaskForChunk(regions: Region[], cx: number, cy: number): Uint8Array | null {
  const cs = BigInt(CHUNK_SIZE);
  const chunkX0 = BigInt(cx) * cs;
  const chunkY0 = BigInt(cy) * cs;
  const chunkX1 = chunkX0 + cs;
  const chunkY1 = chunkY0 + cs;
  let mask: Uint8Array | null = null;
  for (const r of regions) {
    if ((r.flags & FLAG_FROZEN) === 0) continue;
    const rx1 = r.x + BigInt(r.w);
    const ry1 = r.y + BigInt(r.h);
    if (rx1 <= chunkX0 || r.x >= chunkX1 || ry1 <= chunkY0 || r.y >= chunkY1) continue;
    // overlap in chunk-local cell coords
    const lx0 = Number((r.x > chunkX0 ? r.x : chunkX0) - chunkX0);
    const ly0 = Number((r.y > chunkY0 ? r.y : chunkY0) - chunkY0);
    const lx1 = Number((rx1 < chunkX1 ? rx1 : chunkX1) - chunkX0);
    const ly1 = Number((ry1 < chunkY1 ? ry1 : chunkY1) - chunkY0);
    if (mask === null) mask = new Uint8Array(BITS_BYTES);
    for (let y = ly0; y < ly1; y++) {
      const off = y * 8;
      for (let x = lx0; x < lx1; x++) {
        mask[off + (x >> 3)] |= 1 << (x & 7);
      }
    }
  }
  return mask;
}

export function parseRgba(css: string): [number, number, number, number] {
  const m = /^#([0-9a-f]{6})$/i.exec(css.trim());
  if (!m) throw new Error(`bad color: ${css}`);
  const v = parseInt(m[1], 16);
  return [(v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff, 0xff];
}
