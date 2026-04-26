// Currently armed stamp. Click a stamp button to arm; click canvas to place;
// click the same button or press Escape to disarm.

import { Stamp } from './stamps';

export class StampState {
  private current: Stamp | null = null;
  private listeners = new Set<() => void>();

  select(s: Stamp | null) {
    if (this.current === s) return;
    this.current = s;
    for (const fn of this.listeners) fn();
  }

  active(): Stamp | null { return this.current; }

  onChange(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }
}
