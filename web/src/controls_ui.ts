import { makeCollapsible } from './ui';

type Binding = { key: string; desc: string };

const KEYBOARD: ReadonlyArray<Binding> = [
  { key: 'S', desc: 'toggle stamps' },
  { key: 'R', desc: 'rotate stamp' },
  { key: 'Esc', desc: 'clear stamp / cancel selection' },
  { key: 'Enter', desc: 'commit selection' },
];

const MOUSE: ReadonlyArray<Binding> = [
  { key: 'click', desc: 'paint / place stamp' },
  { key: 'alt+click', desc: 'erase' },
  { key: 'ctrl+drag', desc: 'select region' },
  { key: 'click in sel.', desc: 'toggle cell' },
  { key: 'shift+drag', desc: 'pan' },
  { key: 'right-drag', desc: 'pan' },
  { key: 'right-click', desc: 'deselect stamp' },
  { key: 'wheel', desc: 'zoom' },
];

function section(heading: string, bindings: ReadonlyArray<Binding>): string {
  const rows = bindings
    .map(b => `<tr><td>${b.key}</td><td>${b.desc}</td></tr>`)
    .join('');
  return `<h4>${heading}</h4><table>${rows}</table>`;
}

export class ControlsUi {
  constructor(el: HTMLElement) {
    el.innerHTML =
      `<header><i class="icon" data-lucide="keyboard"></i><span>controls</span></header>` +
      `<div class="body">` +
      section('keyboard', KEYBOARD) +
      section('mouse', MOUSE) +
      `</div>`;
    makeCollapsible(el);
  }
}
