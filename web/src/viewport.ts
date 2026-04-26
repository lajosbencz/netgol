// Camera + viewport-to-chunk math. Single source of truth used by both renderer and
// subscription so they cannot disagree about which chunks are visible.

import { CHUNK_SIZE } from './protocol';

export const HALO = 2;

export type Camera = { x: number; y: number; zoom: number };

export type ChunkRange = { c0x: number; c0y: number; c1x: number; c1y: number };

/** Visible chunk range expanded by HALO. Inclusive on both ends. */
export function visibleChunkRange(cam: Camera, vw: number, vh: number): ChunkRange {
  const vpMinX = Math.floor(cam.x - vw / (2 * cam.zoom));
  const vpMinY = Math.floor(cam.y - vh / (2 * cam.zoom));
  const vpMaxX = vpMinX + Math.ceil(vw / cam.zoom); // exclusive
  const vpMaxY = vpMinY + Math.ceil(vh / cam.zoom);
  return {
    c0x: Math.floor(vpMinX / CHUNK_SIZE) - HALO,
    c0y: Math.floor(vpMinY / CHUNK_SIZE) - HALO,
    c1x: Math.floor((vpMaxX - 1) / CHUNK_SIZE) + HALO,
    c1y: Math.floor((vpMaxY - 1) / CHUNK_SIZE) + HALO,
  };
}

/** Cell coords under the given canvas pixel. */
export function screenToWorld(cam: Camera, vw: number, vh: number, px: number, py: number): { x: number; y: number } {
  return {
    x: (px - vw / 2) / cam.zoom + cam.x,
    y: (py - vh / 2) / cam.zoom + cam.y,
  };
}
