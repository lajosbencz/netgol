// Static info panel: keyboard/mouse hint list. Collapsible header.

import { makeCollapsible } from './ui';

const HINTS: ReadonlyArray<string> = [
  'click stamp · click canvas: place',
  'esc: clear stamp / selection',
  'ctrl-drag: select region · enter: commit',
  'click in selection: toggle cell',
  'shift-drag / right-drag: pan · wheel: zoom',
];

export class InfoUi {
  constructor(el: HTMLElement) {
    el.innerHTML =
      `<header><span class="dot blue"></span><span>info</span></header>` +
      `<div class="body">${HINTS.map((h) => `<div>${h}</div>`).join('')}</div>`;
    makeCollapsible(el);
  }
}
