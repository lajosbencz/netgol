// GoL pattern catalog. `#` = alive, anything else = dead.

export type Stamp = { name: string; category: string; pattern: string[] };

export function rotateStampCW(stamp: Stamp): Stamp {
  const rows = stamp.pattern;
  const h = rows.length;
  const w = Math.max(...rows.map(r => r.length));
  const padded = rows.map(r => r.padEnd(w, '.'));
  const newRows: string[] = [];
  for (let x = 0; x < w; x++) {
    let row = '';
    for (let y = h - 1; y >= 0; y--) row += padded[y][x] ?? '.';
    newRows.push(row);
  }
  return { ...stamp, pattern: newRows };
}

export const STAMPS: ReadonlyArray<Stamp> = [
  { name: 'Block', category: 'still', pattern: ['##', '##'] },
  { name: 'Beehive', category: 'still', pattern: ['.##.', '#..#', '.##.'] },
  { name: 'Loaf', category: 'still', pattern: ['.##.', '#..#', '.#.#', '..#.'] },
  { name: 'Boat', category: 'still', pattern: ['##.', '#.#', '.#.'] },

  { name: 'Blinker', category: 'oscillator', pattern: ['###'] },
  { name: 'Toad', category: 'oscillator', pattern: ['.###', '###.'] },
  { name: 'Beacon', category: 'oscillator', pattern: ['##..', '##..', '..##', '..##'] },
  {
    name: 'Pulsar',
    category: 'oscillator',
    pattern: [
      '..###...###..',
      '.............',
      '#....#.#....#',
      '#....#.#....#',
      '#....#.#....#',
      '..###...###..',
      '.............',
      '..###...###..',
      '#....#.#....#',
      '#....#.#....#',
      '#....#.#....#',
      '.............',
      '..###...###..',
    ],
  },

  { name: 'Glider', category: 'spaceship', pattern: ['.#.', '..#', '###'] },
  { name: 'LWSS', category: 'spaceship', pattern: ['.####', '#...#', '....#', '#..#.'] },
  { name: 'MWSS', category: 'spaceship', pattern: ['...#..', '.#...#', '#.....', '#....#', '######'] },

  { name: 'R-pentomino', category: 'methuselah', pattern: ['.##', '##.', '.#.'] },
  { name: 'Acorn', category: 'methuselah', pattern: ['.#.....', '...#...', '##..###'] },
  { name: 'Diehard', category: 'methuselah', pattern: ['......#.', '##......', '.#...###'] },

  {
    name: 'Gosper gun',
    category: 'gun',
    pattern: [
      '........................#...........',
      '......................#.#...........',
      '............##......##............##',
      '...........#...#....##............##',
      '##........#.....#...##..............',
      '##........#...#.##....#.#...........',
      '..........#.....#.......#...........',
      '...........#...#....................',
      '............##......................',
    ],
  },
];

export function stampCells(s: Stamp): { w: number; h: number; cells: Uint8Array } {
  const h = s.pattern.length;
  const w = Math.max(...s.pattern.map((r) => r.length));
  const cells = new Uint8Array(w * h);
  for (let y = 0; y < h; y++) {
    const row = s.pattern[y] ?? '';
    for (let x = 0; x < w; x++) cells[y * w + x] = row[x] === '#' ? 1 : 0;
  }
  return { w, h, cells };
}

/**
 * Top-left world cell where a stamp pattern begins, when its center sits under
 * `cursorX, cursorY`. Both placement and ghost rendering call this so they
 * cannot disagree.
 */
export function stampAnchor(stamp: Stamp, cursorX: number, cursorY: number) {
  const pat = stampCells(stamp);
  return {
    x0: Math.floor(cursorX) - Math.floor(pat.w / 2),
    y0: Math.floor(cursorY) - Math.floor(pat.h / 2),
    w: pat.w,
    h: pat.h,
    cells: pat.cells,
  };
}
