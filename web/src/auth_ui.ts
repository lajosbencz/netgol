import { mountIcons } from './icons';
import { Camera } from './viewport';
import { encodeClient } from './protocol';

export type AuthInfo = {
  uid: number;
  name: string;
  email: string;
  claim: [number, number] | null;
};

export type Provider = { slug: string; name: string };

const ZOOM_MIN = 1;

export class AuthUi {
  private auth: AuthInfo | null = null;
  private claimMode = false;
  private cursorCx = 0;
  private cursorCy = 0;
  private providers: Provider[] = [];
  private providersLoaded = false;
  private modal: HTMLElement | null = null;

  constructor(
    private container: HTMLElement,
    private send: (b: Uint8Array) => void,
    private cam: Camera,
    private claimW: number,
    private claimH: number,
    private scheduleFrame: () => void,
  ) {
    this.render();
    document.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') {
        if (this.modal) { this.closeModal(); return; }
        if (this.claimMode) { this.exitClaimMode(); return; }
      }
    });
  }

  setProviders(providers: Provider[]) {
    this.providers = providers;
    this.providersLoaded = true;
    this.render();
  }

  setAuth(auth: AuthInfo | null) {
    this.auth = auth;
    if (this.claimMode && auth?.claim) this.exitClaimMode();
    this.render();
    this.scheduleFrame();
  }

  get active(): boolean { return this.claimMode; }

  get cursorChunk(): { cx: number; cy: number } | null {
    return this.claimMode ? { cx: this.cursorCx, cy: this.cursorCy } : null;
  }

  get claimPreview(): { cursorCx: number; cursorCy: number; claimW: number; claimH: number } | null {
    if (!this.claimMode) return null;
    return { cursorCx: this.cursorCx, cursorCy: this.cursorCy, claimW: this.claimW, claimH: this.claimH };
  }

  setHover(worldX: number, worldY: number) {
    if (!this.claimMode) return;
    const chunkSize = 64;
    const newCx = Math.floor(worldX / chunkSize);
    const newCy = Math.floor(worldY / chunkSize);
    if (newCx !== this.cursorCx || newCy !== this.cursorCy) {
      this.cursorCx = newCx;
      this.cursorCy = newCy;
      this.scheduleFrame();
    }
  }

  commitClaim() {
    if (!this.claimMode) return;
    const tlCx = this.cursorCx - Math.floor(this.claimW / 2);
    const tlCy = this.cursorCy - Math.floor(this.claimH / 2);
    this.send(encodeClient({ kind: 'ClaimCreate', cx: tlCx, cy: tlCy }));
    this.exitClaimMode();
  }

  onClaimResult(ok: boolean) {
    if (!ok) {
      console.warn('claim rejected by server');
      this.render();
    }
  }

  private enterClaimMode() {
    this.claimMode = true;
    this.cam.zoom = ZOOM_MIN;
    this.render();
    this.scheduleFrame();
  }

  exitClaimMode() {
    this.claimMode = false;
    this.render();
    this.scheduleFrame();
  }

  private openModal() {
    if (this.modal) return;
    const overlay = document.createElement('div');
    overlay.className = 'modal-overlay';
    overlay.addEventListener('click', (e) => {
      if (e.target === overlay) this.closeModal();
    });
    const box = document.createElement('div');
    box.className = 'modal-box';
    const title = document.createElement('div');
    title.className = 'modal-title';
    title.textContent = 'Sign in to claim a region';
    const closeBtn = document.createElement('button');
    closeBtn.className = 'modal-close icon-btn';
    closeBtn.setAttribute('aria-label', 'Close');
    closeBtn.innerHTML = '<i data-lucide="x"></i>';
    closeBtn.addEventListener('click', () => this.closeModal());
    title.appendChild(closeBtn);
    box.appendChild(title);
    for (const p of this.providers) {
      const btn = document.createElement('a');
      btn.className = 'provider-btn';
      btn.href = `/auth/${p.slug}`;
      btn.innerHTML = `<i data-lucide="log-in"></i><span>${p.name}</span>`;
      box.appendChild(btn);
    }
    if (this.providers.length === 0) {
      const msg = document.createElement('p');
      msg.className = 'modal-empty';
      msg.textContent = 'No login providers configured.';
      box.appendChild(msg);
    }
    overlay.appendChild(box);
    document.body.appendChild(overlay);
    this.modal = overlay;
    mountIcons();
  }

  private closeModal() {
    this.modal?.remove();
    this.modal = null;
  }

  private render() {
    this.container.innerHTML = '';
    if (this.auth && this.auth.uid !== 0) {
      this.renderAuthenticated();
    } else {
      this.renderAnonymous();
    }
    mountIcons();
  }

  private renderAnonymous() {
    if (this.claimMode) return;
    if (!this.providersLoaded || this.providers.length === 0) return;
    const btn = document.createElement('button');
    btn.className = 'leave-mark-btn';
    btn.textContent = 'Leave your mark!';
    btn.addEventListener('click', () => this.openModal());
    this.container.appendChild(btn);
  }

  private renderAuthenticated() {
    const auth = this.auth!;
    const wrap = document.createElement('div');
    wrap.className = 'auth-controls';
    const nameEl = document.createElement('span');
    nameEl.className = 'auth-name';
    nameEl.textContent = auth.name || auth.email;
    wrap.appendChild(nameEl);
    if (this.claimMode) {
      const cancel = document.createElement('button');
      cancel.className = 'icon-btn';
      cancel.setAttribute('aria-label', 'Cancel');
      cancel.innerHTML = '<i data-lucide="x"></i><span>Cancel</span>';
      cancel.addEventListener('click', () => this.exitClaimMode());
      wrap.appendChild(cancel);
    } else if (auth.claim) {
      const release = document.createElement('button');
      release.className = 'icon-btn';
      release.setAttribute('aria-label', 'Release claim');
      release.innerHTML = '<i data-lucide="trash-2"></i><span>Release</span>';
      release.addEventListener('click', () => {
        this.send(encodeClient({ kind: 'ClaimDelete' }));
      });
      wrap.appendChild(release);
      const move = document.createElement('button');
      move.className = 'icon-btn';
      move.setAttribute('aria-label', 'Move claim');
      move.innerHTML = '<i data-lucide="map-pin"></i><span>Move</span>';
      move.addEventListener('click', () => this.enterClaimMode());
      wrap.appendChild(move);
    } else {
      const claim = document.createElement('button');
      claim.className = 'icon-btn';
      claim.setAttribute('aria-label', 'Claim region');
      claim.innerHTML = '<i data-lucide="map-pin"></i><span>Claim</span>';
      claim.addEventListener('click', () => this.enterClaimMode());
      wrap.appendChild(claim);
    }
    this.container.appendChild(wrap);
  }
}
