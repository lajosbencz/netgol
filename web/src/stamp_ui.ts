// Stamp picker panel. Click to arm, click again or Esc to clear. Active stamp
// follows the cursor as a ghost (rendered in render.ts) and is placed on click.

import { STAMPS, Stamp } from './stamps';
import { StampState } from './stamp_state';
import { makeCollapsible } from './ui';

export class StampUi {
  private buttons = new Map<string, HTMLButtonElement>();

  constructor(private el: HTMLElement, private state: StampState) {
    this.render();
    state.onChange(() => this.refresh());
    this.refresh();
  }

  private render() {
    const cats = new Map<string, Stamp[]>();
    for (const s of STAMPS) {
      const arr = cats.get(s.category) ?? [];
      arr.push(s);
      cats.set(s.category, arr);
    }

    this.el.innerHTML =
      `<header><i class="icon" data-lucide="shapes"></i><span>stamps</span></header>` +
      `<div class="body"></div>`;
    const body = this.el.querySelector('.body') as HTMLElement;

    for (const [cat, items] of cats) {
      const h = document.createElement('h4');
      h.textContent = cat;
      body.appendChild(h);
      for (const s of items) {
        const btn = document.createElement('button');
        btn.dataset.name = s.name;
        btn.textContent = s.name;
        btn.addEventListener('click', (e) => {
          e.stopPropagation();
          const cur = this.state.active();
          this.state.select(cur?.name === s.name ? null : s);
        });
        body.appendChild(btn);
        this.buttons.set(s.name, btn);
      }
    }

    makeCollapsible(this.el);
  }

  private refresh() {
    const activeName = this.state.active()?.name ?? null;
    for (const [name, btn] of this.buttons) {
      btn.classList.toggle('active', name === activeName);
    }
  }
}
