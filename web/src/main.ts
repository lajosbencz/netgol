import { decodeServer, ServerMsg } from './protocol';
import { Camera } from './viewport';
import { ChunkCache, parseRgba, Palette } from './world';
import { Renderer } from './render';
import { Subscription } from './subscription';
import { createControls } from './controls';
import { Hud, ConnState } from './ui';
import { Selection } from './selection';
import { StampUi } from './stamp_ui';
import { StampState } from './stamp_state';
import { InfoUi } from './info_ui';
import { applyUrlToCamera, UrlSync } from './url_sync';
import { mountIcons } from './icons';

const styles = getComputedStyle(document.documentElement);
const cssVar = (name: string, fallback: string) =>
  styles.getPropertyValue(name).trim() || fallback;
const BG = cssVar('--bg', '#0f1729');
const ALIVE = cssVar('--alive', '#c8ccd4');
const FROZEN_ALIVE = cssVar('--frozen-alive', '#d4a574');
const FROZEN_DEAD = cssVar('--frozen-dead', '#1f2940');
const ACCENT = cssVar('--accent', '#4a7fb8');

const palette: Palette = {
  alive: parseRgba(ALIVE),
  frozenAlive: parseRgba(FROZEN_ALIVE),
  frozenDead: parseRgba(FROZEN_DEAD),
};

const canvas = document.getElementById('canvas') as HTMLCanvasElement;
const statsEl = document.getElementById('stats') as HTMLElement;
const stampsEl = document.getElementById('stamps') as HTMLElement;
const infoEl = document.getElementById('info') as HTMLElement;

const cam: Camera = { x: 0, y: 0, zoom: 3 };
applyUrlToCamera(cam);
const urlSync = new UrlSync(cam);
// 512 chunks is ~8 MB of OffscreenCanvas backing memory at 64x64 - comfortably above
// any reasonable viewport+halo (a 32x32 chunk grid at extreme zoom is the cap), so the
// LRU only kicks in after sustained panning across distinct world regions.
const cache = new ChunkCache(512, palette);
const renderer = new Renderer(canvas, BG, ALIVE, ACCENT);
const hud = new Hud(statsEl);
const selection = new Selection();
const stampState = new StampState();
new InfoUi(infoEl);

let liveChunks = 0;
let tickRateHz = 0;
let tickUtilization = 0;
let pendingFrame = false;

function scheduleFrame() {
  if (pendingFrame) return;
  pendingFrame = true;
  requestAnimationFrame(frame);
}

function connState(): ConnState {
  switch (ws.readyState) {
    case WebSocket.OPEN: return 'connected';
    case WebSocket.CONNECTING: return 'connecting';
    default: return 'disconnected';
  }
}

function frame() {
  pendingFrame = false;
  const rect = canvas.getBoundingClientRect();
  if (canvas.style.width !== `${rect.width}px` || canvas.style.height !== `${rect.height}px`) {
    renderer.resize(rect.width, rect.height);
  }
  controls.flushEdits();
  subscription.request(cam, rect.width, rect.height);
  const stamp = stampState.active();
  const hover = controls.hoverCell();
  const ghost = stamp && hover ? { stamp, x: hover.x, y: hover.y } : null;
  renderer.render(cam, cache, selection, ghost);
  urlSync.tick();
  hud.set({
    conn: connState(),
    liveChunks,
    cachedChunks: cache.size(),
    tickRateHz,
    tickUtilization,
    camX: cam.x,
    camY: cam.y,
    zoom: cam.zoom,
  });
}

function handleResize() {
  const rect = canvas.getBoundingClientRect();
  renderer.resize(rect.width, rect.height);
  scheduleFrame();
}
window.addEventListener('resize', handleResize);
handleResize();

window.addEventListener('hashchange', () => {
  if (applyUrlToCamera(cam)) {
    urlSync.noteExternalWrite();
    subscription.flush();
    scheduleFrame();
  }
});

const wsUrl = (() => {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  return `${proto}://${location.host}/ws`;
})();

let ws: WebSocket;
let reconnectAttempt = 0;
let reconnectTimer: number | null = null;

// Log-spaced backoff: equally spaced on a log scale between min and max,
// reaching the ceiling around attempt 6. ±25% jitter avoids thundering herds.
const RECONNECT_MIN_MS = 3000;
const RECONNECT_MAX_MS = 30000;
const RECONNECT_STEPS = 6;
const RECONNECT_FACTOR = Math.pow(RECONNECT_MAX_MS / RECONNECT_MIN_MS, 1 / RECONNECT_STEPS);
function reconnectDelay(attempt: number): number {
  const base = Math.min(RECONNECT_MAX_MS, RECONNECT_MIN_MS * Math.pow(RECONNECT_FACTOR, attempt));
  return base * (0.75 + Math.random() * 0.5);
}

const send = (bytes: Uint8Array) => {
  if (!ws || ws.readyState !== WebSocket.OPEN) return;
  const ab = new ArrayBuffer(bytes.byteLength);
  new Uint8Array(ab).set(bytes);
  ws.send(ab);
};

const subscription = new Subscription(send, cache);
const onSettle = () => {
  subscription.flush();
  scheduleFrame();
};
const controls = createControls(canvas, cam, send, scheduleFrame, onSettle, selection, cache, stampState);
new StampUi(stampsEl, stampState);
mountIcons();
selection.onChange(scheduleFrame);
stampState.onChange(scheduleFrame);

function connect() {
  reconnectTimer = null;
  ws = new WebSocket(wsUrl);
  ws.binaryType = 'arraybuffer';

  ws.addEventListener('open', () => {
    reconnectAttempt = 0;
    // A frame may have already run while the socket was CONNECTING - it would have
    // skipped the wire send but still recorded the desired set as `current`, leaving
    // the server with zero subscriptions. Force a re-sub now.
    subscription.reset();
    scheduleFrame();
  });
  ws.addEventListener('close', (e) => {
    liveChunks = 0;
    tickRateHz = 0;
    tickUtilization = 0;
    cache.clear();
    subscription.reset();
    scheduleFrame();
    if (reconnectTimer === null) {
      const delay = reconnectDelay(reconnectAttempt++);
      console.warn(`websocket closed (code=${e.code}, reason=${e.reason || 'n/a'}, clean=${e.wasClean}); reconnect attempt ${reconnectAttempt} in ${Math.round(delay)}ms`);
      reconnectTimer = window.setTimeout(connect, delay);
    }
  });
  ws.addEventListener('error', () => {
    console.warn('websocket error');
    scheduleFrame();
  });
  ws.addEventListener('message', (e) => {
    const msg: ServerMsg = decodeServer(e.data as ArrayBuffer);
    switch (msg.kind) {
      case 'Hello':
        break;
      case 'ChunkState':
      case 'ChunkDelta':
        cache.put(msg.cx, msg.cy, msg.tick, msg.bits);
        break;
      case 'Regions':
        cache.setRegions(msg.regions);
        break;
      case 'Reaped':
        cache.drop(msg.cx, msg.cy);
        break;
      case 'Stats':
        liveChunks = msg.liveChunks;
        tickRateHz = msg.tickRateHz;
        tickUtilization = msg.tickUtilization;
        break;
    }
    scheduleFrame();
  });
}

connect();

// Render loop is driven by network and input. Also tick at 30 Hz to refresh HUD/anim
// even if traffic is quiet.
setInterval(scheduleFrame, 33);
