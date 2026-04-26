// Rectangular selection overlay. Lives client-side; on commit, contents are flushed
// as a batch of Edits.

import { encodeClient, EditCell } from './protocol';
import { Stamp, stampCells } from './stamps';
import { ChunkCache } from './world';
import { worldToCellAddr } from './coords';

export const MAX_SIZE = 64;

export type Bounds = { x: number; y: number; w: number; h: number };

type State = { bounds: Bounds; cells: Uint8Array };

export class Selection {
  private state: State | null = null;
  /** In-progress drag, in world cell coords. */
  private drag: { x0: number; y0: number; x1: number; y1: number } | null = null;
  private listeners = new Set<() => void>();

  onChange(fn: () => void) { this.listeners.add(fn); return () => this.listeners.delete(fn); }
  private emit() { for (const fn of this.listeners) fn(); }

  isActive(): boolean { return this.state !== null; }
  bounds(): Bounds | null { return this.state?.bounds ?? null; }
  cells(): Uint8Array | null { return this.state?.cells ?? null; }
  dragRect(): Bounds | null {
    if (!this.drag) return null;
    return normalize(this.drag.x0, this.drag.y0, this.drag.x1, this.drag.y1);
  }

  beginDrag(wx: number, wy: number) {
    this.cancel();
    this.drag = { x0: wx, y0: wy, x1: wx, y1: wy };
    this.emit();
  }

  updateDrag(wx: number, wy: number) {
    if (!this.drag) return;
    this.drag.x1 = wx;
    this.drag.y1 = wy;
    this.emit();
  }

  endDrag(cache: ChunkCache) {
    if (!this.drag) return;
    const r = normalize(this.drag.x0, this.drag.y0, this.drag.x1, this.drag.y1);
    this.drag = null;
    if (r.w === 0 || r.h === 0) { this.emit(); return; }
    const w = Math.min(r.w, MAX_SIZE);
    const h = Math.min(r.h, MAX_SIZE);
    const bounds: Bounds = { x: r.x, y: r.y, w, h };
    const cells = new Uint8Array(w * h);
    for (let row = 0; row < h; row++) {
      for (let col = 0; col < w; col++) {
        const ax = bounds.x + col;
        const ay = bounds.y + row;
        cells[row * w + col] = readCell(cache, ax, ay);
      }
    }
    this.state = { bounds, cells };
    this.emit();
  }

  toggleAt(ax: number, ay: number): boolean {
    if (!this.state) return false;
    const { bounds, cells } = this.state;
    const col = ax - bounds.x;
    const row = ay - bounds.y;
    if (col < 0 || row < 0 || col >= bounds.w || row >= bounds.h) return false;
    const i = row * bounds.w + col;
    cells[i] = cells[i] ? 0 : 1;
    this.emit();
    return true;
  }

  contains(ax: number, ay: number): boolean {
    if (!this.state) return false;
    const { bounds } = this.state;
    return ax >= bounds.x && ay >= bounds.y && ax < bounds.x + bounds.w && ay < bounds.y + bounds.h;
  }

  applyStamp(stamp: Stamp) {
    if (!this.state) return;
    const { bounds, cells } = this.state;
    const pat = stampCells(stamp);
    cells.fill(0);
    const offX = Math.max(0, Math.floor((bounds.w - pat.w) / 2));
    const offY = Math.max(0, Math.floor((bounds.h - pat.h) / 2));
    const copyW = Math.min(pat.w, bounds.w - offX);
    const copyH = Math.min(pat.h, bounds.h - offY);
    for (let r = 0; r < copyH; r++) {
      for (let c = 0; c < copyW; c++) {
        cells[(offY + r) * bounds.w + (offX + c)] = pat.cells[r * pat.w + c];
      }
    }
    this.emit();
  }

  commit(send: (bytes: Uint8Array) => void) {
    if (!this.state) return;
    const { bounds, cells } = this.state;
    const out: EditCell[] = [];
    for (let row = 0; row < bounds.h; row++) {
      for (let col = 0; col < bounds.w; col++) {
        const a = worldToCellAddr(bounds.x + col, bounds.y + row);
        out.push({ ...a, alive: cells[row * bounds.w + col] === 1 });
      }
    }
    // Edit message uses u16 count; cap at 65535. 64*64=4096 is fine.
    send(encodeClient({ kind: 'Edit', cells: out }));
    this.state = null;
    this.emit();
  }

  cancel() {
    if (this.state || this.drag) {
      this.state = null;
      this.drag = null;
      this.emit();
    }
  }
}

function readCell(cache: ChunkCache, ax: number, ay: number): number {
  const a = worldToCellAddr(ax, ay);
  const e = cache.get(a.cx, a.cy);
  if (!e) return 0;
  const byte = e.bits[a.ly * 8 + (a.lx >> 3)];
  return (byte >> (a.lx & 7)) & 1;
}

function normalize(x0: number, y0: number, x1: number, y1: number): Bounds {
  const x = Math.floor(Math.min(x0, x1));
  const y = Math.floor(Math.min(y0, y1));
  const xe = Math.floor(Math.max(x0, x1)) + 1;
  const ye = Math.floor(Math.max(y0, y1)) + 1;
  return { x, y, w: xe - x, h: ye - y };
}
