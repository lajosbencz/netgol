// Desktop gesture recognizer.
//
//   Plain click/drag      → paint (alt = erase), or place stamp on click if armed
//   Shift-drag / right-mb → pan
//   Right-click           → clear armed stamp
//   Wheel                 → zoom around cursor
//   Ctrl-drag             → make a (max 64x64) selection
//   Click in selection    → toggle that overlay cell
//   Enter                 → commit selection
//   Escape                → cancel selection / clear stamp

import { Actions } from './actions';

export class DesktopControls {
  private dragging = false;
  private painting = false;
  private selecting = false;
  private toggling = false;
  private paintAlive = true;
  private lastX = 0;
  private lastY = 0;
  private toggled = new Set<string>();
  private wheelEndTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(private canvas: HTMLCanvasElement, private a: Actions) {
    canvas.addEventListener('pointerdown', this.onDown);
    canvas.addEventListener('pointermove', this.onMove);
    canvas.addEventListener('pointerup', this.onUp);
    canvas.addEventListener('pointercancel', this.onUp);
    canvas.addEventListener('pointerleave', this.onLeave);
    canvas.addEventListener('wheel', this.onWheel, { passive: false });
    canvas.addEventListener('contextmenu', (e) => e.preventDefault());
    window.addEventListener('keydown', this.onKey);
  }

  private onDown = (e: PointerEvent) => {
    this.canvas.setPointerCapture(e.pointerId);
    this.lastX = e.clientX;
    this.lastY = e.clientY;
    const w = this.a.worldAt(e.clientX, e.clientY);
    this.a.setHover(w.x, w.y);

    if (e.ctrlKey || e.metaKey) {
      this.selecting = true;
      this.a.beginSelection(w.x, w.y);
      return;
    }
    if (e.button === 2 && this.a.stamps.active()) {
      this.a.clearStamp();
      return;
    }
    if (e.button === 2 || e.shiftKey) {
      this.dragging = true;
      return;
    }
    if (this.a.selection.isActive()) {
      this.toggling = true;
      this.toggled.clear();
      this.dedupedToggle(w.x, w.y);
      return;
    }
    const stamp = this.a.stamps.active();
    if (stamp) {
      this.a.placeStamp(stamp, w.x, w.y);
      return;
    }
    this.painting = true;
    this.paintAlive = !e.altKey;
    this.a.paintAt(w.x, w.y, this.paintAlive);
  };

  private onMove = (e: PointerEvent) => {
    const w = this.a.worldAt(e.clientX, e.clientY);
    this.a.setHover(w.x, w.y);

    if (this.dragging) {
      const dx = e.clientX - this.lastX;
      const dy = e.clientY - this.lastY;
      this.lastX = e.clientX;
      this.lastY = e.clientY;
      this.a.panBy(dx, dy);
    } else if (this.selecting) {
      this.a.updateSelection(w.x, w.y);
    } else if (this.toggling) {
      this.dedupedToggle(w.x, w.y);
    } else if (this.painting) {
      this.a.paintAt(w.x, w.y, this.paintAlive);
    }
  };

  private onUp = (e: PointerEvent) => {
    this.canvas.releasePointerCapture(e.pointerId);
    const wasInteracting = this.dragging || this.painting || this.toggling || this.selecting;
    this.dragging = false;
    this.painting = false;
    this.toggling = false;
    if (this.selecting) {
      this.selecting = false;
      this.a.endSelection();
    }
    if (wasInteracting) this.a.notifySettle();
  };

  private onLeave = () => {
    this.a.clearHover();
  };

  private onWheel = (e: WheelEvent) => {
    e.preventDefault();
    const factor = Math.exp(-e.deltaY * 0.0015);
    this.a.zoomBy(factor, e.clientX - this.a.canvasRect().left, e.clientY - this.a.canvasRect().top);
    // Wheel events arrive in bursts with no native "end" event. Fire onSettle
    // 150 ms after the last wheel event so the subscription manager flushes.
    if (this.wheelEndTimer) clearTimeout(this.wheelEndTimer);
    this.wheelEndTimer = setTimeout(() => {
      this.wheelEndTimer = null;
      this.a.notifySettle();
    }, 150);
  };

  private onKey = (e: KeyboardEvent) => {
    if (e.key === 'Escape') {
      this.a.cancelSelection();
      this.a.clearStamp();
    } else if (e.key === 'Enter' && this.a.selection.isActive()) {
      this.a.commitSelection();
    } else if (e.key === 's' || e.key === 'S') {
      document.getElementById('stamps')?.classList.toggle('collapsed');
    } else if ((e.key === 'r' || e.key === 'R') && this.a.stamps.active()) {
      this.a.rotateStamp();
    }
  };

  private dedupedToggle(wx: number, wy: number) {
    const ax = Math.floor(wx);
    const ay = Math.floor(wy);
    const k = `${ax},${ay}`;
    if (this.toggled.has(k)) return;
    this.toggled.add(k);
    this.a.toggleInSelection(wx, wy);
  }
}
