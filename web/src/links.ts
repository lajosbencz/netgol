const LINKS: { href: string; icon: string; label: string; aria: string }[] = [
  {
    href: 'https://github.com/lajosbencz/netgol',
    icon: 'code-xml',
    label: 'source',
    aria: 'Source on GitHub',
  },
  {
    href: 'https://en.wikipedia.org/wiki/Conway%27s_Game_of_Life',
    icon: 'book-open',
    label: "Conway's Game of Life",
    aria: "Conway's Game of Life on Wikipedia",
  },
];

export class Links {
  constructor(container: HTMLElement) {
    for (const { href, icon, label, aria } of LINKS) {
      const a = document.createElement('a');
      a.className = 'link-item';
      a.href = href;
      a.target = '_blank';
      a.rel = 'noopener';
      a.ariaLabel = aria;
      a.innerHTML = `<i class="icon" data-lucide="${icon}"></i><span>${label}</span>`;
      container.appendChild(a);
    }
  }
}
