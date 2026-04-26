// Tracks the desired chunk subscription set. Throttled to one wire-side update
// per `THROTTLE_MS` (matches sim tick), with an explicit `flush()` for end-of-
// pan/zoom that bypasses the throttle.

import { Camera, visibleChunkRange } from './viewport';
import { encodeClient } from './protocol';
import { ChunkCache } from './world';
import { packCoord, unpackCoord } from './coords';

/** Extra cache slots beyond the live subscription set, for back-and-forth pan. */
const CACHE_BUFFER = 128;
/** Minimum interval between wire-side reconciles. Matches the 10 Hz sim tick. */
const THROTTLE_MS = 100;

export class Subscription {
  private current = new Set<number>();
  /** Last camera+viewport snapshot. Skip the whole diff if nothing changed. */
  private last: { x: number; y: number; zoom: number; vw: number; vh: number } | null = null;
  /** Most recent request args; consumed by the next fire. */
  private pending: { cam: Camera; vw: number; vh: number } | null = null;
  private lastFireMs = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;

  constructor(
    private send: (bytes: Uint8Array) => void,
    private cache: ChunkCache,
  ) {}

  /**
   * Queue a reconcile with the server. Fires immediately if the last fire was
   * more than `THROTTLE_MS` ago; otherwise schedules a trailing fire. Cheap to
   * call per frame.
   */
  request(cam: Camera, vw: number, vh: number) {
    this.pending = { cam, vw, vh };
    if (this.timer !== null) return;
    const elapsed = performance.now() - this.lastFireMs;
    if (elapsed >= THROTTLE_MS) {
      this.fire();
    } else {
      this.timer = setTimeout(() => {
        this.timer = null;
        this.fire();
      }, THROTTLE_MS - elapsed);
    }
  }

  /** Force-fire any pending request immediately. Call on pan/zoom release. */
  flush() {
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    this.fire();
  }

  private fire() {
    if (!this.pending) return;
    const { cam, vw, vh } = this.pending;
    this.pending = null;
    this.lastFireMs = performance.now();
    this.update(cam, vw, vh);
  }

  private update(cam: Camera, vw: number, vh: number) {
    if (
      this.last !== null &&
      this.last.x === cam.x && this.last.y === cam.y && this.last.zoom === cam.zoom &&
      this.last.vw === vw && this.last.vh === vh
    ) {
      return;
    }
    this.last = { x: cam.x, y: cam.y, zoom: cam.zoom, vw, vh };

    // Every chunk overlapping viewport+halo, regardless of zoom. Bounded
    // intrinsically by ZOOM_MIN x canvas size - fits comfortably under the
    // wire u16 coord-list count at any allowed zoom.
    const range = visibleChunkRange(cam, vw, vh);
    const next = new Set<number>();
    for (let cy = range.c0y; cy <= range.c1y; cy++) {
      for (let cx = range.c0x; cx <= range.c1x; cx++) {
        next.add(packCoord(cx, cy));
      }
    }

    const toSub: Array<[number, number]> = [];
    const toUnsub: Array<[number, number]> = [];
    for (const key of next) if (!this.current.has(key)) toSub.push(unpackCoord(key));
    for (const key of this.current) if (!next.has(key)) toUnsub.push(unpackCoord(key));

    this.current = next;

    // Size the cache to the current viewport+halo plus a buffer so a quick pan-
    // back doesn't re-fetch a just-unsubscribed chunk (Subscribe → ChunkState is
    // ~100ms; during the gap the chunk would render as background).
    // The cache LRU evicts the oldest entries automatically once we exceed cap.
    this.cache.setCapacity(next.size + CACHE_BUFFER);

    if (toUnsub.length > 0) {
      this.send(encodeClient({ kind: 'Unsubscribe', coords: toUnsub }));
    }
    if (toSub.length > 0) this.send(encodeClient({ kind: 'Subscribe', coords: toSub }));
  }

  reset() {
    this.current.clear();
    this.last = null;
    this.pending = null;
    this.lastFireMs = 0;
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }
}

