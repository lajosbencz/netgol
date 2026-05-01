// Canvas renderer. One drawImage per visible chunk; the chunk's OffscreenCanvas
// is the source. Single render path for any zoom; smoothing toggles between
// crisp (zoom >= 1) and smooth (zoom < 1). Optionally draws a selection overlay
// and an armed-stamp ghost.

import { CHUNK_SIZE } from './protocol';
import { Camera, visibleChunkRange } from './viewport';
import { ChunkCache } from './world';
import { Selection } from './selection';
import { Stamp, stampAnchor } from './stamps';

export type Ghost = { stamp: Stamp; x: number; y: number } | null;
export type ClaimPreview = { cursorCx: number; cursorCy: number; claimW: number; claimH: number } | null;

export class Renderer {
  private ctx: CanvasRenderingContext2D;
  private bg: string;
  private alive: string;
  private accent: string;
  private ownedAlive: string;

  constructor(private canvas: HTMLCanvasElement, bgCss: string, aliveCss: string, accentCss: string, ownedAliveCss: string) {
    const ctx = canvas.getContext('2d', { alpha: false });
    if (!ctx) throw new Error('no 2d context');
    this.ctx = ctx;
    this.bg = bgCss;
    this.alive = aliveCss;
    this.accent = accentCss;
    this.ownedAlive = ownedAliveCss;
  }

  resize(w: number, h: number) {
    const dpr = window.devicePixelRatio || 1;
    this.canvas.width = Math.round(w * dpr);
    this.canvas.height = Math.round(h * dpr);
    this.canvas.style.width = `${w}px`;
    this.canvas.style.height = `${h}px`;
  }

  render(cam: Camera, cache: ChunkCache, selection: Selection, ghost: Ghost, claimPreview: ClaimPreview = null) {
    const { ctx, canvas } = this;
    const dpr = window.devicePixelRatio || 1;
    const vw = canvas.width / dpr;
    const vh = canvas.height / dpr;

    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = this.bg;
    ctx.fillRect(0, 0, vw, vh);

    ctx.imageSmoothingEnabled = cam.zoom < 1;

    const range = visibleChunkRange(cam, vw, vh);
    const cs = CHUNK_SIZE * cam.zoom;
    const ox = vw / 2 - cam.x * cam.zoom;
    const oy = vh / 2 - cam.y * cam.zoom;

    // Iterate the cache (bounded), not the visible range (unbounded at extreme
    // zoom-out). Range check keeps off-screen chunks out of drawImage.
    for (const e of cache.entries()) {
      if (e.cx < range.c0x || e.cx > range.c1x || e.cy < range.c0y || e.cy > range.c1y) continue;
      const dx = ox + e.cx * cs;
      const dy = oy + e.cy * cs;
      ctx.drawImage(e.canvas, dx, dy, cs, cs);
    }

    this.drawSelection(cam, vw, vh, selection);
    if (claimPreview) this.drawClaimPreview(cam, vw, vh, claimPreview);
    if (ghost) this.drawGhost(cam, vw, vh, ghost);
  }

  private drawSelection(cam: Camera, vw: number, vh: number, selection: Selection) {
    const { ctx } = this;
    const z = cam.zoom;
    const ox = vw / 2 - cam.x * z;
    const oy = vh / 2 - cam.y * z;

    const drag = selection.dragRect();
    if (drag) {
      ctx.save();
      ctx.lineWidth = 1;
      ctx.setLineDash([4, 4]);
      ctx.strokeStyle = this.accent;
      ctx.strokeRect(ox + drag.x * z, oy + drag.y * z, drag.w * z, drag.h * z);
      ctx.restore();
    }

    const b = selection.bounds();
    const cells = selection.cells();
    if (!b || !cells) return;

    ctx.save();
    ctx.globalAlpha = 0.1;
    ctx.fillStyle = this.accent;
    ctx.fillRect(ox + b.x * z, oy + b.y * z, b.w * z, b.h * z);
    ctx.globalAlpha = 1;
    // Overlay reflects the user's pending state, which may differ from world.
    ctx.fillStyle = this.alive;
    for (let row = 0; row < b.h; row++) {
      for (let col = 0; col < b.w; col++) {
        if (cells[row * b.w + col] !== 1) continue;
        ctx.fillRect(ox + (b.x + col) * z, oy + (b.y + row) * z, z, z);
      }
    }
    ctx.lineWidth = 1.5;
    ctx.strokeStyle = this.accent;
    ctx.strokeRect(ox + b.x * z, oy + b.y * z, b.w * z, b.h * z);
    ctx.restore();
  }

  private drawClaimPreview(cam: Camera, vw: number, vh: number, p: NonNullable<ClaimPreview>) {
    const { ctx } = this;
    const z = cam.zoom;
    const cs = CHUNK_SIZE * z;
    const ox = vw / 2 - cam.x * z;
    const oy = vh / 2 - cam.y * z;
    const tlCx = p.cursorCx - Math.floor(p.claimW / 2);
    const tlCy = p.cursorCy - Math.floor(p.claimH / 2);
    const px = ox + tlCx * cs;
    const py = oy + tlCy * cs;
    const pw = p.claimW * cs;
    const ph = p.claimH * cs;
    ctx.save();
    ctx.globalAlpha = 0.2;
    ctx.fillStyle = this.ownedAlive;
    ctx.fillRect(px, py, pw, ph);
    ctx.globalAlpha = 1;
    ctx.lineWidth = 2;
    ctx.setLineDash([6, 4]);
    ctx.strokeStyle = this.ownedAlive;
    ctx.strokeRect(px, py, pw, ph);
    ctx.restore();
  }

  private drawGhost(cam: Camera, vw: number, vh: number, ghost: NonNullable<Ghost>) {
    const { ctx } = this;
    const z = cam.zoom;
    const ox = vw / 2 - cam.x * z;
    const oy = vh / 2 - cam.y * z;

    const anc = stampAnchor(ghost.stamp, ghost.x, ghost.y);

    ctx.save();
    ctx.globalAlpha = 0.5;
    ctx.fillStyle = this.alive;
    for (let r = 0; r < anc.h; r++) {
      for (let c = 0; c < anc.w; c++) {
        if (anc.cells[r * anc.w + c] !== 1) continue;
        ctx.fillRect(ox + (anc.x0 + c) * z, oy + (anc.y0 + r) * z, z, z);
      }
    }
    ctx.globalAlpha = 1;
    ctx.lineWidth = 1;
    ctx.setLineDash([3, 3]);
    ctx.strokeStyle = this.accent;
    ctx.strokeRect(ox + anc.x0 * z, oy + anc.y0 * z, anc.w * z, anc.h * z);
    ctx.restore();
  }
}
