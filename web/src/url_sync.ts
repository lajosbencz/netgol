// Two-way sync between the URL fragment and the camera.
// Format: `#x,y,z` where x,y are integer cell coords and z is the zoom.
// Apply-from-URL on load and on hashchange (link sharing); write-to-URL on
// pan/zoom settle (debounced) using replaceState so the back button is unaffected.

import { Camera } from './viewport';

const ZOOM_MIN = 1 / 4;
const ZOOM_MAX = 32;
const ZOOM_DECIMALS = 3;

function clamp(v: number, lo: number, hi: number) {
  return Math.max(lo, Math.min(hi, v));
}

function parseHash(): { x: number; y: number; zoom?: number } | null {
  const h = location.hash.replace(/^#/, '');
  if (!h) return null;
  const parts = h.split(',');
  if (parts.length < 2) return null;
  const x = Number(parts[0]);
  const y = Number(parts[1]);
  if (!Number.isFinite(x) || !Number.isFinite(y)) return null;
  let zoom: number | undefined;
  if (parts.length >= 3) {
    const z = Number(parts[2]);
    if (!Number.isFinite(z)) return null;
    zoom = clamp(z, ZOOM_MIN, ZOOM_MAX);
  }
  return { x: Math.round(x), y: Math.round(y), zoom };
}

function format(cam: Camera): string {
  const x = Math.round(cam.x);
  const y = Math.round(cam.y);
  const z = Number(cam.zoom.toFixed(ZOOM_DECIMALS));
  return `#${x},${y},${z}`;
}

export function applyUrlToCamera(cam: Camera): boolean {
  const p = parseHash();
  if (!p) return false;
  cam.x = p.x;
  cam.y = p.y;
  if (p.zoom !== undefined) cam.zoom = p.zoom;
  return true;
}

export class UrlSync {
  private lastSnapshot = '';
  private lastWritten = '';
  private steadySince = 0;

  constructor(private cam: Camera, private debounceMs: number = 350) {
    this.lastWritten = location.hash;
  }

  tick(now: number = performance.now()) {
    const snap = format(this.cam);
    if (snap !== this.lastSnapshot) {
      this.lastSnapshot = snap;
      this.steadySince = now;
      return;
    }
    if (snap === this.lastWritten) return;
    if (now - this.steadySince < this.debounceMs) return;
    history.replaceState(null, '', snap);
    this.lastWritten = snap;
  }

  noteExternalWrite() {
    this.lastWritten = location.hash;
    this.lastSnapshot = format(this.cam);
  }
}
