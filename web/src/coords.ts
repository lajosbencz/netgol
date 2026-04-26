// Coordinate helpers shared across renderer, controls, subscription, selection.
// Centralized so chunk math has a single source of truth - divergence between
// any two callers (e.g. cull vs. subscription) would be an invisible bug.

import { CHUNK_SIZE } from './protocol';

/** World-space cell address, decomposed into chunk + local. */
export type CellAddr = { cx: number; cy: number; lx: number; ly: number };

export function worldToCellAddr(ax: number, ay: number): CellAddr {
  const cx = Math.floor(ax / CHUNK_SIZE);
  const cy = Math.floor(ay / CHUNK_SIZE);
  const lx = ((ax % CHUNK_SIZE) + CHUNK_SIZE) % CHUNK_SIZE;
  const ly = ((ay % CHUNK_SIZE) + CHUNK_SIZE) % CHUNK_SIZE;
  return { cx, cy, lx, ly };
}

// Numeric subscription-set key. Packs (cx, cy) into a single safe integer so
// `Set<number>` / `Map<number, …>` dedupe without per-insert string allocation.
// Range bounded at ±2^23 chunks per axis (≥5x10^8 cells); inputs outside that
// range panic so the limit isn't silently wrong.
const PACK_OFF = 1 << 23;
const PACK_MULT = 1 << 24;

export function packCoord(cx: number, cy: number): number {
  if (cx < -PACK_OFF || cx >= PACK_OFF || cy < -PACK_OFF || cy >= PACK_OFF) {
    throw new Error(`chunk coord out of range: (${cx}, ${cy})`);
  }
  return (cx + PACK_OFF) * PACK_MULT + (cy + PACK_OFF);
}

export function unpackCoord(p: number): [number, number] {
  const cy = (p % PACK_MULT) - PACK_OFF;
  const cx = Math.floor(p / PACK_MULT) - PACK_OFF;
  return [cx, cy];
}
