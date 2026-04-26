// Picks a gesture recognizer based on the primary input modality. Both classes
// register their own DOM listeners; this module only wires up the shared
// Actions surface used by `main.ts` for per-frame queries (flushEdits / hoverCell).

import { Camera } from '../viewport';
import { Selection } from '../selection';
import { ChunkCache } from '../world';
import { StampState } from '../stamp_state';
import { Actions } from './actions';
import { DesktopControls } from './desktop';
import { MobileControls } from './mobile';

export function createControls(
  canvas: HTMLCanvasElement,
  cam: Camera,
  send: (bytes: Uint8Array) => void,
  onChange: () => void,
  onSettle: () => void,
  selection: Selection,
  cache: ChunkCache,
  stamps: StampState,
): Actions {
  const actions = new Actions(canvas, cam, send, onChange, onSettle, selection, cache, stamps);
  if (isTouchPrimary()) new MobileControls(canvas, actions);
  else new DesktopControls(canvas, actions);
  return actions;
}

function isTouchPrimary(): boolean {
  return window.matchMedia('(hover: none) and (pointer: coarse)').matches;
}

export { Actions } from './actions';
