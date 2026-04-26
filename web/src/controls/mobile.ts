// Mobile gesture recognizer.
//
//   1-finger tap            → paint cell, or place armed stamp, or toggle a selected cell
//   1-finger drag           → paint stroke (or toggle stroke if inside an active selection)
//   1-finger drag w/ stamp  → ghost follows finger; release places the stamp
//   1-finger long-press     → arm selection drag from press point; further motion sizes it
//   2-finger pinch          → zoom around midpoint
//   2-finger drag           → pan (combined with pinch)
//   FAB esc / enter         → cancel / commit (replaces keyboard)

import { Actions } from './actions';

const LONG_PRESS_MS = 500;
const TAP_MOVE_PX = 8;

type Touch = { id: number; sx: number; sy: number; cx: number; cy: number };

type State =
  | { kind: 'idle' }
  | { kind: 'pending'; t: Touch; timer: ReturnType<typeof setTimeout> }
  | { kind: 'stamp_drag' }
  | { kind: 'stroke'; mode: 'paint' | 'toggle'; toggled: Set<string> }
  | { kind: 'select_drag' }
  | { kind: 'pinch'; midX: number; midY: number; dist: number };

export class MobileControls {
  private touches = new Map<number, Touch>();
  private state: State = { kind: 'idle' };
  private fab: HTMLDivElement;
  private fabEsc: HTMLButtonElement;
  private fabEnter: HTMLButtonElement;

  constructor(private canvas: HTMLCanvasElement, private a: Actions) {
    canvas.addEventListener('pointerdown', this.onDown);
    canvas.addEventListener('pointermove', this.onMove);
    canvas.addEventListener('pointerup', this.onUp);
    canvas.addEventListener('pointercancel', this.onUp);
    canvas.addEventListener('contextmenu', (e) => e.preventDefault());
    canvas.style.touchAction = 'none';

    const fab = document.createElement('div');
    fab.id = 'fab';
    fab.className = 'panel';
    const esc = document.createElement('button');
    esc.textContent = 'esc';
    esc.addEventListener('click', () => {
      this.a.cancelSelection();
      this.a.clearStamp();
    });
    const enter = document.createElement('button');
    enter.textContent = 'enter';
    enter.addEventListener('click', () => this.a.commitSelection());
    fab.appendChild(esc);
    fab.appendChild(enter);
    document.body.appendChild(fab);
    this.fab = fab;
    this.fabEsc = esc;
    this.fabEnter = enter;

    this.a.selection.onChange(this.refreshFab);
    this.a.stamps.onChange(this.refreshFab);
    this.refreshFab();
  }

  private refreshFab = () => {
    const showEsc = this.a.selection.isActive() || this.a.selection.dragRect() !== null || this.a.stamps.active() !== null;
    const showEnter = this.a.selection.isActive();
    this.fabEsc.style.display = showEsc ? '' : 'none';
    this.fabEnter.style.display = showEnter ? '' : 'none';
    this.fab.style.display = showEsc || showEnter ? '' : 'none';
  };

  private onDown = (e: PointerEvent) => {
    if (e.pointerType !== 'touch') return;
    if (this.touches.size >= 2) return;
    e.preventDefault();
    this.canvas.setPointerCapture(e.pointerId);
    const t: Touch = { id: e.pointerId, sx: e.clientX, sy: e.clientY, cx: e.clientX, cy: e.clientY };
    this.touches.set(e.pointerId, t);

    if (this.touches.size === 2) {
      this.startPinch();
      return;
    }

    if (this.a.stamps.active()) {
      this.state = { kind: 'stamp_drag' };
      const w = this.a.worldAt(t.cx, t.cy);
      this.a.setHover(w.x, w.y);
      return;
    }

    const timer = setTimeout(() => this.promoteLongPress(), LONG_PRESS_MS);
    this.state = { kind: 'pending', t, timer };
  };

  private onMove = (e: PointerEvent) => {
    if (e.pointerType !== 'touch') return;
    const t = this.touches.get(e.pointerId);
    if (!t) return;
    t.cx = e.clientX;
    t.cy = e.clientY;
    const w = this.a.worldAt(t.cx, t.cy);

    switch (this.state.kind) {
      case 'pending': {
        if (Math.hypot(t.cx - t.sx, t.cy - t.sy) <= TAP_MOVE_PX) return;
        clearTimeout(this.state.timer);
        const insideSel =
          this.a.selection.isActive() && this.a.selection.contains(Math.floor(w.x), Math.floor(w.y));
        const mode: 'paint' | 'toggle' = insideSel ? 'toggle' : 'paint';
        const toggled = new Set<string>();
        this.state = { kind: 'stroke', mode, toggled };
        this.applyStroke(this.state, w.x, w.y);
        return;
      }
      case 'stroke':
        this.applyStroke(this.state, w.x, w.y);
        return;
      case 'stamp_drag':
        this.a.setHover(w.x, w.y);
        return;
      case 'select_drag':
        this.a.updateSelection(w.x, w.y);
        return;
      case 'pinch': {
        const ts = [...this.touches.values()];
        if (ts.length < 2) return;
        const [pa, pb] = ts;
        const midX = (pa.cx + pb.cx) / 2;
        const midY = (pa.cy + pb.cy) / 2;
        const dist = Math.hypot(pa.cx - pb.cx, pa.cy - pb.cy);
        this.a.panBy(midX - this.state.midX, midY - this.state.midY);
        if (this.state.dist > 0) {
          const rect = this.a.canvasRect();
          this.a.zoomBy(dist / this.state.dist, midX - rect.left, midY - rect.top);
        }
        this.state.midX = midX;
        this.state.midY = midY;
        this.state.dist = dist;
        return;
      }
    }
  };

  private onUp = (e: PointerEvent) => {
    if (e.pointerType !== 'touch') return;
    const t = this.touches.get(e.pointerId);
    if (!t) return;
    this.canvas.releasePointerCapture(e.pointerId);
    this.touches.delete(e.pointerId);

    const prev = this.state;
    this.state = { kind: 'idle' };

    switch (prev.kind) {
      case 'pending': {
        clearTimeout(prev.timer);
        const w = this.a.worldAt(t.cx, t.cy);
        const ax = Math.floor(w.x);
        const ay = Math.floor(w.y);
        const stamp = this.a.stamps.active();
        if (stamp) this.a.placeStamp(stamp, w.x, w.y);
        else if (this.a.selection.isActive() && this.a.selection.contains(ax, ay))
          this.a.toggleInSelection(w.x, w.y);
        else this.a.paintAt(w.x, w.y, true);
        this.a.notifySettle();
        return;
      }
      case 'stroke':
        this.a.notifySettle();
        return;
      case 'stamp_drag': {
        this.a.clearHover();
        const stamp = this.a.stamps.active();
        if (stamp) {
          const w = this.a.worldAt(t.cx, t.cy);
          this.a.placeStamp(stamp, w.x, w.y);
        }
        this.a.notifySettle();
        return;
      }
      case 'select_drag':
        this.a.endSelection();
        this.a.notifySettle();
        return;
      case 'pinch':
        this.a.notifySettle();
        return;
    }
  };

  private applyStroke(s: { mode: 'paint' | 'toggle'; toggled: Set<string> }, wx: number, wy: number) {
    const ax = Math.floor(wx);
    const ay = Math.floor(wy);
    if (s.mode === 'paint') {
      this.a.paintAt(wx, wy, true);
      return;
    }
    const k = `${ax},${ay}`;
    if (s.toggled.has(k)) return;
    s.toggled.add(k);
    this.a.toggleInSelection(wx, wy);
  }

  private promoteLongPress() {
    if (this.state.kind !== 'pending') return;
    const t = this.state.t;
    this.state = { kind: 'select_drag' };
    const w = this.a.worldAt(t.cx, t.cy);
    this.a.beginSelection(w.x, w.y);
  }

  private startPinch() {
    // Whatever the first finger was doing, abandon it - pinch wins.
    if (this.state.kind === 'pending') clearTimeout(this.state.timer);
    if (this.state.kind === 'select_drag') this.a.cancelSelection();
    if (this.state.kind === 'stamp_drag') this.a.clearHover();
    const ts = [...this.touches.values()];
    const [pa, pb] = ts;
    const midX = (pa.cx + pb.cx) / 2;
    const midY = (pa.cy + pb.cy) / 2;
    const dist = Math.hypot(pa.cx - pb.cx, pa.cy - pb.cy);
    this.state = { kind: 'pinch', midX, midY, dist };
  }
}
