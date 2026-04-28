// Shared mutation surface used by both desktop and mobile gesture recognizers.
// Owns the pending paint queue and hover cell. Gesture classes never touch
// camera / selection / stamps directly - they call methods here.

import { encodeClient, EditCell } from '../protocol';
import { Camera, screenToWorld } from '../viewport';
import { Selection } from '../selection';
import { ChunkCache } from '../world';
import { StampState } from '../stamp_state';
import { Stamp, stampAnchor } from '../stamps';
import { worldToCellAddr } from '../coords';
import { ZOOM_MIN, ZOOM_MAX } from '../viewport';

export class Actions {
  private pending = new Map<string, EditCell>();
  private hover: { x: number; y: number } | null = null;

  constructor(
    private canvas: HTMLCanvasElement,
    private cam: Camera,
    private send: (bytes: Uint8Array) => void,
    private onChange: () => void,
    private onSettle: () => void,
    public readonly selection: Selection,
    private cache: ChunkCache,
    public readonly stamps: StampState,
  ) {}

  flushEdits() {
    if (this.pending.size === 0) return;
    const cells = Array.from(this.pending.values());
    this.pending.clear();
    this.send(encodeClient({ kind: 'Edit', cells }));
  }

  hoverCell(): { x: number; y: number } | null { return this.hover; }

  setHover(wx: number, wy: number) {
    const x = Math.floor(wx);
    const y = Math.floor(wy);
    if (this.hover && this.hover.x === x && this.hover.y === y) return;
    this.hover = { x, y };
    this.onChange();
  }

  clearHover() {
    if (this.hover === null) return;
    this.hover = null;
    this.onChange();
  }

  worldAt(clientX: number, clientY: number): { x: number; y: number } {
    const rect = this.canvas.getBoundingClientRect();
    return screenToWorld(this.cam, rect.width, rect.height, clientX - rect.left, clientY - rect.top);
  }

  paintAt(wx: number, wy: number, alive: boolean) {
    const a = worldToCellAddr(Math.floor(wx), Math.floor(wy));
    this.pending.set(`${a.cx},${a.cy},${a.lx},${a.ly}`, { ...a, alive });
  }

  placeStamp(stamp: Stamp, wx: number, wy: number) {
    const anc = stampAnchor(stamp, wx, wy);
    const cells: EditCell[] = [];
    for (let r = 0; r < anc.h; r++) {
      for (let c = 0; c < anc.w; c++) {
        const a = worldToCellAddr(anc.x0 + c, anc.y0 + r);
        cells.push({ ...a, alive: anc.cells[r * anc.w + c] === 1 });
      }
    }
    this.send(encodeClient({ kind: 'Edit', cells }));
  }

  beginSelection(wx: number, wy: number) { this.selection.beginDrag(wx, wy); }
  updateSelection(wx: number, wy: number) { this.selection.updateDrag(wx, wy); }
  endSelection() { this.selection.endDrag(this.cache); }
  toggleInSelection(wx: number, wy: number) {
    this.selection.toggleAt(Math.floor(wx), Math.floor(wy));
  }
  commitSelection() { this.selection.commit(this.send); }
  cancelSelection() { this.selection.cancel(); }
  clearStamp() { this.stamps.select(null); }
  rotateStamp() { this.stamps.rotate(); }

  panBy(screenDx: number, screenDy: number) {
    this.cam.x -= screenDx / this.cam.zoom;
    this.cam.y -= screenDy / this.cam.zoom;
    this.onChange();
  }

  zoomBy(factor: number, screenX: number, screenY: number) {
    const newZoom = clamp(this.cam.zoom * factor, ZOOM_MIN, ZOOM_MAX);
    if (newZoom === this.cam.zoom) return;
    const rect = this.canvas.getBoundingClientRect();
    const px = screenX;
    const py = screenY;
    const before = screenToWorld(this.cam, rect.width, rect.height, px, py);
    this.cam.zoom = newZoom;
    const after = screenToWorld(this.cam, rect.width, rect.height, px, py);
    this.cam.x += before.x - after.x;
    this.cam.y += before.y - after.y;
    this.onChange();
  }

  notifySettle() { this.onSettle(); }

  canvasRect(): DOMRect { return this.canvas.getBoundingClientRect(); }
}

function clamp(v: number, lo: number, hi: number) { return Math.max(lo, Math.min(hi, v)); }
