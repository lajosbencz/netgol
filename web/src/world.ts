// Chunk cache. Keyed by "cx,cy". Lifetime is independent of subscription set -
// only `Reaped`, LRU eviction, and disconnect drop entries. Renderer reads from this
// cache; staleness across a pan is preferred over flicker.
//
// Per-chunk frozen masks are materialised locally from the region table the
// server sends in `Regions`. They're not on the per-chunk wire frame.

import { CHUNK_SIZE, BITS_BYTES, FLAG_FROZEN, Region } from './protocol';

export type ChunkKey = string;
export const key = (cx: number, cy: number): ChunkKey => `${cx},${cy}`;

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

  put(cx: number, cy: number, tick: bigint, bits: Uint8Array) {
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
    this.evictIfNeeded();
  }

  private repaint(e: ChunkEntry) {
    const ctx = e.canvas.getContext('2d')!;
    if (!this.scratch) this.scratch = ctx.createImageData(CHUNK_SIZE, CHUNK_SIZE);
    paintInto(this.scratch, e.bits, e.frozenMask, this.palette);
    ctx.putImageData(this.scratch, 0, 0);
  }

  drop(cx: number, cy: number) { this.map.delete(key(cx, cy)); }

  clear() { this.map.clear(); }

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
