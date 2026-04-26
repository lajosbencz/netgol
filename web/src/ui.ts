// Stats panel: connection-status header + collapsible body with server stats
// and viewport position/zoom.

export type ConnState = 'connecting' | 'connected' | 'disconnected';

export type HudState = {
  conn: ConnState;
  liveChunks: number;
  cachedChunks: number;
  tickRateHz: number;
  tickUtilization: number;
  camX: number;
  camY: number;
  zoom: number;
};

export class Hud {
  private dot: HTMLElement;
  private statusText: HTMLElement;
  private vChunks: HTMLElement;
  private vRate: HTMLElement;
  private vUtil: HTMLElement;
  private vPos: HTMLElement;
  private vZoom: HTMLElement;

  constructor(el: HTMLElement) {
    el.innerHTML =
      `<header>` +
      `<span class="dot"></span>` +
      `<span class="status-text"></span>` +
      `</header>` +
      `<div class="body">` +
      `<div>chunks <span class="v-chunks"></span></div>` +
      `<div>rate <span class="v-rate"></span> hz</div>` +
      `<div>util <span class="v-util"></span>%</div>` +
      `<div>pos <span class="v-pos"></span></div>` +
      `<div>zoom <span class="v-zoom"></span>%</div>` +
      `</div>`;
    this.dot = el.querySelector('.dot') as HTMLElement;
    this.statusText = el.querySelector('.status-text') as HTMLElement;
    this.vChunks = el.querySelector('.v-chunks') as HTMLElement;
    this.vRate = el.querySelector('.v-rate') as HTMLElement;
    this.vUtil = el.querySelector('.v-util') as HTMLElement;
    this.vPos = el.querySelector('.v-pos') as HTMLElement;
    this.vZoom = el.querySelector('.v-zoom') as HTMLElement;
    makeCollapsible(el);
  }

  set(s: HudState) {
    const { dotClass, label } = connVisuals(s.conn);
    if (this.dot.className !== `dot ${dotClass}`) this.dot.className = `dot ${dotClass}`;
    setText(this.statusText, label);
    setText(this.vChunks, `${s.cachedChunks}/${s.liveChunks}`);
    setText(this.vRate, s.tickRateHz.toFixed(1));
    setText(this.vUtil, (s.tickUtilization * 100).toFixed(1));
    setText(this.vPos, `${Math.round(s.camX)}, ${Math.round(s.camY)}`);
    setText(this.vZoom, (s.zoom * 100).toFixed(0));
  }
}

export function makeCollapsible(panel: HTMLElement) {
  const header = panel.querySelector(':scope > header');
  if (!header) return;
  header.addEventListener('click', () => panel.classList.toggle('collapsed'));
}

function setText(el: HTMLElement, t: string) {
  if (el.textContent !== t) el.textContent = t;
}

function connVisuals(c: ConnState): { dotClass: string; label: string } {
  switch (c) {
    case 'connected': return { dotClass: 'green', label: 'connected' };
    case 'connecting': return { dotClass: 'orange', label: 'connecting' };
    case 'disconnected': return { dotClass: 'red', label: 'disconnected' };
  }
}
