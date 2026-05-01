// Wire codec. Mirrors Rust `protocol::lib`.
// Little-endian, fixed layouts, one tag byte per frame.

export const CHUNK_SIZE = 64;
export const BITS_BYTES = CHUNK_SIZE * 8;

const TAG_CHUNK_STATE = 0x01;
const TAG_CHUNK_DELTA = 0x02;
const TAG_REAPED = 0x03;
const TAG_STATS = 0x04;
const TAG_HELLO = 0x05;
const TAG_REGIONS = 0x06;
const TAG_SYNC = 0x07;
const TAG_EDIT_APPLIED = 0x08;
const TAG_SUBSCRIBE = 0x10;
const TAG_UNSUBSCRIBE = 0x11;
const TAG_EDIT = 0x12;

export const FLAG_FROZEN = 1 << 0;

export type Region = {
  x: bigint; y: bigint;
  w: number; h: number;
  flags: number;
  owner: number;
};

export type ServerMsg =
  | { kind: 'Hello'; tick: bigint; chunkSize: number }
  | { kind: 'Regions'; regions: Region[] }
  | { kind: 'ChunkState'; cx: number; cy: number; tick: bigint; bits: Uint8Array }
  | { kind: 'ChunkDelta'; cx: number; cy: number; tick: bigint; bits: Uint8Array }
  | { kind: 'Reaped'; cx: number; cy: number }
  | { kind: 'Stats'; tick: bigint; liveChunks: number; tickRateHz: number; tickUtilization: number }
  | { kind: 'Sync'; tick: bigint }
  | { kind: 'EditApplied'; cx: number; cy: number; cells: EditCell[] };

export type EditCell = { cx: number; cy: number; lx: number; ly: number; alive: boolean };

export type ClientMsg =
  | { kind: 'Subscribe'; coords: Array<[number, number]> }
  | { kind: 'Unsubscribe'; coords: Array<[number, number]> }
  | { kind: 'Edit'; cells: EditCell[] };

export function decodeServer(buf: ArrayBuffer): ServerMsg {
  const r = new Reader(buf);
  const tag = r.u8();
  switch (tag) {
    case TAG_HELLO:
      return { kind: 'Hello', tick: r.u64(), chunkSize: r.u8() };
    case TAG_CHUNK_STATE: {
      const cx = r.i32(); const cy = r.i32(); const tick = r.u64(); const bits = r.bytes(BITS_BYTES);
      return { kind: 'ChunkState', cx, cy, tick, bits };
    }
    case TAG_CHUNK_DELTA: {
      const cx = r.i32(); const cy = r.i32(); const tick = r.u64(); const bits = r.bytes(BITS_BYTES);
      return { kind: 'ChunkDelta', cx, cy, tick, bits };
    }
    case TAG_REGIONS: {
      const n = r.u16();
      const regions: Region[] = [];
      for (let i = 0; i < n; i++) {
        regions.push({
          x: r.i64(), y: r.i64(),
          w: r.u32(), h: r.u32(),
          flags: r.u8(),
          owner: r.u32(),
        });
      }
      return { kind: 'Regions', regions };
    }
    case TAG_REAPED:
      return { kind: 'Reaped', cx: r.i32(), cy: r.i32() };
    case TAG_STATS:
      return {
        kind: 'Stats',
        tick: r.u64(),
        liveChunks: r.u32(),
        tickRateHz: r.f32(),
        tickUtilization: r.f32(),
      };
    case TAG_SYNC:
      return { kind: 'Sync', tick: r.u64() };
    case TAG_EDIT_APPLIED: {
      const cx = r.i32(), cy = r.i32(), count = r.u16();
      const cells: EditCell[] = [];
      for (let i = 0; i < count; i++) {
        const lx = r.u8(), ly = r.u8(), alive = r.u8() !== 0;
        cells.push({ cx, cy, lx, ly, alive });
      }
      return { kind: 'EditApplied', cx, cy, cells };
    }
    default:
      throw new Error(`unknown server tag 0x${tag.toString(16)}`);
  }
}

export function encodeClient(msg: ClientMsg): Uint8Array {
  const w = new Writer();
  switch (msg.kind) {
    case 'Subscribe':
      w.u8(TAG_SUBSCRIBE); writeCoordList(w, msg.coords); break;
    case 'Unsubscribe':
      w.u8(TAG_UNSUBSCRIBE); writeCoordList(w, msg.coords); break;
    case 'Edit':
      w.u8(TAG_EDIT); w.u16(msg.cells.length);
      for (const c of msg.cells) {
        w.i32(c.cx); w.i32(c.cy); w.u8(c.lx); w.u8(c.ly); w.u8(c.alive ? 1 : 0);
      }
      break;
  }
  return w.finish();
}

function writeCoordList(w: Writer, coords: Array<[number, number]>) {
  w.u16(coords.length);
  for (const [x, y] of coords) { w.i32(x); w.i32(y); }
}

class Reader {
  private dv: DataView;
  private u8a: Uint8Array;
  private pos = 0;
  private len: number;
  constructor(buf: ArrayBuffer) {
    this.dv = new DataView(buf);
    this.u8a = new Uint8Array(buf);
    this.len = buf.byteLength;
  }
  private need(n: number) {
    if (this.pos + n > this.len) {
      throw new Error(`truncated frame: need ${n} bytes at offset ${this.pos}, have ${this.len}`);
    }
  }
  u8() { this.need(1); return this.dv.getUint8(this.pos++); }
  u16() { this.need(2); const v = this.dv.getUint16(this.pos, true); this.pos += 2; return v; }
  u32() { this.need(4); const v = this.dv.getUint32(this.pos, true); this.pos += 4; return v; }
  i32() { this.need(4); const v = this.dv.getInt32(this.pos, true); this.pos += 4; return v; }
  f32() { this.need(4); const v = this.dv.getFloat32(this.pos, true); this.pos += 4; return v; }
  u64() { this.need(8); const v = this.dv.getBigUint64(this.pos, true); this.pos += 8; return v; }
  i64() { this.need(8); const v = this.dv.getBigInt64(this.pos, true); this.pos += 8; return v; }
  bytes(n: number) { this.need(n); const s = this.u8a.slice(this.pos, this.pos + n); this.pos += n; return s; }
}

class Writer {
  private chunks: number[] = [];
  u8(v: number) { this.chunks.push(v & 0xff); }
  u16(v: number) { this.chunks.push(v & 0xff, (v >> 8) & 0xff); }
  u32(v: number) {
    this.chunks.push(v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff, (v >> 24) & 0xff);
  }
  i32(v: number) { this.u32(v >>> 0); }
  finish(): Uint8Array { return new Uint8Array(this.chunks); }
}
