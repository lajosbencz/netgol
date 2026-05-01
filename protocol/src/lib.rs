//! Wire protocol. Little-endian binary frames; one tag byte + fixed-layout payload.
//!
//! `Chunk` payloads carry the bitset as `CHUNK_SIZE` rows x 8 bytes (LE u64), totalling
//! [`BITS_BYTES`]. All decode errors collapse to a single [`DecodeError`] - fail early.

use simulation::{ChunkCoord, CHUNK_SIZE};
use std::sync::Arc;

pub const BITS_BYTES: usize = CHUNK_SIZE * 8;

// Server → client tags
const TAG_CHUNK_STATE: u8 = 0x01;
const TAG_CHUNK_DELTA: u8 = 0x02;
const TAG_REAPED: u8 = 0x03;
const TAG_STATS: u8 = 0x04;
const TAG_HELLO: u8 = 0x05;
const TAG_REGIONS: u8 = 0x06;
const TAG_SYNC: u8 = 0x07;
const TAG_EDIT_APPLIED: u8 = 0x08;
const TAG_AUTH_STATE: u8 = 0x09;
const TAG_CLAIM_RESULT: u8 = 0x0A;

/// Region flags (bitfield, packed into `u8`).
pub const FLAG_FROZEN: u8 = 1 << 0; // cells inside don't evolve
pub const FLAG_LOCKED: u8 = 1 << 1; // cells inside cannot be Edited
pub const FLAG_OWNED:  u8 = 1 << 2; // `owner` field is meaningful
// bits 3..7 reserved.

// Client → server tags
const TAG_SUBSCRIBE: u8 = 0x10;
const TAG_UNSUBSCRIBE: u8 = 0x11;
const TAG_EDIT: u8 = 0x12;
const TAG_CLAIM_CREATE: u8 = 0x13;
const TAG_CLAIM_DELETE: u8 = 0x14;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: i64,
    pub y: i64,
    pub w: u32,
    pub h: u32,
    pub flags: u8,
    /// User id of the owner. Meaningful iff `flags & FLAG_OWNED != 0`.
    pub owner: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerMsg {
    Hello { tick: u64, chunk_size: u8 },
    /// World region table. Sent once after `Hello` and again whenever it changes.
    /// The client materializes per-chunk flag masks from this list for rendering.
    Regions { regions: Arc<[Region]> },
    ChunkState { cx: i32, cy: i32, tick: u64, bits: [u8; BITS_BYTES] },
    ChunkDelta { cx: i32, cy: i32, tick: u64, bits: [u8; BITS_BYTES] },
    Reaped { cx: i32, cy: i32 },
    /// Periodic broadcast of server-wide stats.
    Stats {
        tick: u64,
        live_chunks: u32,
        tick_rate_hz: f32,
        tick_utilization: f32,
    },
    /// Per-tick heartbeat. Clients advance their local simulation on receipt.
    Sync { tick: u64 },
    /// Broadcast to chunk subscribers when a player edit is applied. Clients
    /// patch their local bits then let the next `Sync` drive the GoL step.
    /// Cells are all in the same chunk (cx, cy); wire encodes only lx/ly/alive.
    EditApplied { cx: i32, cy: i32, cells: Vec<EditCell> },
    /// Sent after WebSocket join and after any auth/claim state change.
    /// `uid == 0` means anonymous. `claim` is `None` when the user has no active claim.
    AuthState {
        uid: u32,
        claim: Option<ChunkCoord>,
        name: String,
        email: String,
    },
    /// Response to a `ClaimCreate` or `ClaimDelete` client message.
    ClaimResult { ok: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    Subscribe(Vec<(i32, i32)>),
    Unsubscribe(Vec<(i32, i32)>),
    Edit(Vec<EditCell>),
    ClaimCreate(ChunkCoord),
    ClaimDelete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditCell {
    pub cx: i32,
    pub cy: i32,
    pub lx: u8,
    pub ly: u8,
    pub alive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Empty,
    UnknownTag(u8),
    Truncated,
    BadLocalCoord,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty frame"),
            Self::UnknownTag(t) => write!(f, "unknown tag 0x{t:02x}"),
            Self::Truncated => write!(f, "truncated frame"),
            Self::BadLocalCoord => write!(f, "local coord out of CHUNK_SIZE range"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub fn encode_server(msg: &ServerMsg, out: &mut Vec<u8>) {
    match msg {
        ServerMsg::Hello { tick, chunk_size } => {
            out.push(TAG_HELLO);
            out.extend_from_slice(&tick.to_le_bytes());
            out.push(*chunk_size);
        }
        ServerMsg::ChunkState { cx, cy, tick, bits } => {
            out.push(TAG_CHUNK_STATE);
            write_chunk_payload(out, *cx, *cy, *tick, bits);
        }
        ServerMsg::ChunkDelta { cx, cy, tick, bits } => {
            out.push(TAG_CHUNK_DELTA);
            write_chunk_payload(out, *cx, *cy, *tick, bits);
        }
        ServerMsg::Regions { regions } => {
            out.push(TAG_REGIONS);
            let n = u16::try_from(regions.len()).expect("region count >= 65536");
            out.extend_from_slice(&n.to_le_bytes());
            for r in regions.iter() {
                out.extend_from_slice(&r.x.to_le_bytes());
                out.extend_from_slice(&r.y.to_le_bytes());
                out.extend_from_slice(&r.w.to_le_bytes());
                out.extend_from_slice(&r.h.to_le_bytes());
                out.push(r.flags);
                out.extend_from_slice(&r.owner.to_le_bytes());
            }
        }
        ServerMsg::Reaped { cx, cy } => {
            out.push(TAG_REAPED);
            out.extend_from_slice(&cx.to_le_bytes());
            out.extend_from_slice(&cy.to_le_bytes());
        }
        ServerMsg::Stats { tick, live_chunks, tick_rate_hz, tick_utilization } => {
            out.push(TAG_STATS);
            out.extend_from_slice(&tick.to_le_bytes());
            out.extend_from_slice(&live_chunks.to_le_bytes());
            out.extend_from_slice(&tick_rate_hz.to_le_bytes());
            out.extend_from_slice(&tick_utilization.to_le_bytes());
        }
        ServerMsg::Sync { tick } => {
            out.push(TAG_SYNC);
            out.extend_from_slice(&tick.to_le_bytes());
        }
        ServerMsg::EditApplied { cx, cy, cells } => {
            out.push(TAG_EDIT_APPLIED);
            out.extend_from_slice(&cx.to_le_bytes());
            out.extend_from_slice(&cy.to_le_bytes());
            let n = u16::try_from(cells.len()).expect("edit list >= 65536 cells");
            out.extend_from_slice(&n.to_le_bytes());
            for c in cells {
                out.push(c.lx);
                out.push(c.ly);
                out.push(u8::from(c.alive));
            }
        }
        ServerMsg::AuthState { uid, claim, name, email } => {
            out.push(TAG_AUTH_STATE);
            out.extend_from_slice(&uid.to_le_bytes());
            match claim {
                Some((cx, cy)) => {
                    out.push(1u8);
                    out.extend_from_slice(&cx.to_le_bytes());
                    out.extend_from_slice(&cy.to_le_bytes());
                }
                None => {
                    out.push(0u8);
                    out.extend_from_slice(&0i32.to_le_bytes());
                    out.extend_from_slice(&0i32.to_le_bytes());
                }
            }
            let nb = u16::try_from(name.len()).expect("name >= 65536 bytes");
            out.extend_from_slice(&nb.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            let eb = u16::try_from(email.len()).expect("email >= 65536 bytes");
            out.extend_from_slice(&eb.to_le_bytes());
            out.extend_from_slice(email.as_bytes());
        }
        ServerMsg::ClaimResult { ok } => {
            out.push(TAG_CLAIM_RESULT);
            out.push(u8::from(*ok));
        }
    }
}

pub fn decode_server(buf: &[u8]) -> Result<ServerMsg, DecodeError> {
    let mut r = Reader::new(buf);
    let tag = r.u8()?;
    match tag {
        TAG_HELLO => Ok(ServerMsg::Hello {
            tick: r.u64()?,
            chunk_size: r.u8()?,
        }),
        TAG_CHUNK_STATE => {
            let (cx, cy, tick, bits) = r.chunk_payload()?;
            Ok(ServerMsg::ChunkState { cx, cy, tick, bits })
        }
        TAG_CHUNK_DELTA => {
            let (cx, cy, tick, bits) = r.chunk_payload()?;
            Ok(ServerMsg::ChunkDelta { cx, cy, tick, bits })
        }
        TAG_REGIONS => {
            let n = r.u16()? as usize;
            let mut regions = Vec::with_capacity(n);
            for _ in 0..n {
                regions.push(Region {
                    x: r.i64()?,
                    y: r.i64()?,
                    w: r.u32()?,
                    h: r.u32()?,
                    flags: r.u8()?,
                    owner: r.u32()?,
                });
            }
            Ok(ServerMsg::Regions { regions: Arc::from(regions) })
        }
        TAG_REAPED => Ok(ServerMsg::Reaped { cx: r.i32()?, cy: r.i32()? }),
        TAG_STATS => Ok(ServerMsg::Stats {
            tick: r.u64()?,
            live_chunks: r.u32()?,
            tick_rate_hz: r.f32()?,
            tick_utilization: r.f32()?,
        }),
        TAG_SYNC => Ok(ServerMsg::Sync { tick: r.u64()? }),
        TAG_EDIT_APPLIED => {
            let cx = r.i32()?;
            let cy = r.i32()?;
            let n = r.u16()? as usize;
            let mut cells = Vec::with_capacity(n);
            for _ in 0..n {
                let lx = r.u8()?;
                let ly = r.u8()?;
                let alive = r.u8()? != 0;
                if (lx as usize) >= CHUNK_SIZE || (ly as usize) >= CHUNK_SIZE {
                    return Err(DecodeError::BadLocalCoord);
                }
                cells.push(EditCell { cx, cy, lx, ly, alive });
            }
            Ok(ServerMsg::EditApplied { cx, cy, cells })
        }
        TAG_AUTH_STATE => {
            let uid = r.u32()?;
            let has_claim = r.u8()? != 0;
            let claim_cx = r.i32()?;
            let claim_cy = r.i32()?;
            let claim = if has_claim { Some((claim_cx, claim_cy)) } else { None };
            let nb = r.u16()? as usize;
            let name = String::from_utf8(r.take(nb)?.to_vec())
                .map_err(|_| DecodeError::Truncated)?;
            let eb = r.u16()? as usize;
            let email = String::from_utf8(r.take(eb)?.to_vec())
                .map_err(|_| DecodeError::Truncated)?;
            Ok(ServerMsg::AuthState { uid, claim, name, email })
        }
        TAG_CLAIM_RESULT => Ok(ServerMsg::ClaimResult { ok: r.u8()? != 0 }),
        t => Err(DecodeError::UnknownTag(t)),
    }
}

pub fn encode_client(msg: &ClientMsg, out: &mut Vec<u8>) {
    match msg {
        ClientMsg::Subscribe(coords) => {
            out.push(TAG_SUBSCRIBE);
            write_coord_list(out, coords);
        }
        ClientMsg::Unsubscribe(coords) => {
            out.push(TAG_UNSUBSCRIBE);
            write_coord_list(out, coords);
        }
        ClientMsg::Edit(cells) => {
            out.push(TAG_EDIT);
            let n = u16::try_from(cells.len()).expect("edit list >= 65536 cells");
            out.extend_from_slice(&n.to_le_bytes());
            for c in cells {
                out.extend_from_slice(&c.cx.to_le_bytes());
                out.extend_from_slice(&c.cy.to_le_bytes());
                out.push(c.lx);
                out.push(c.ly);
                out.push(u8::from(c.alive));
            }
        }
        ClientMsg::ClaimCreate((cx, cy)) => {
            out.push(TAG_CLAIM_CREATE);
            out.extend_from_slice(&cx.to_le_bytes());
            out.extend_from_slice(&cy.to_le_bytes());
        }
        ClientMsg::ClaimDelete => {
            out.push(TAG_CLAIM_DELETE);
        }
    }
}

pub fn decode_client(buf: &[u8]) -> Result<ClientMsg, DecodeError> {
    let mut r = Reader::new(buf);
    let tag = r.u8()?;
    match tag {
        TAG_SUBSCRIBE => Ok(ClientMsg::Subscribe(r.coord_list()?)),
        TAG_UNSUBSCRIBE => Ok(ClientMsg::Unsubscribe(r.coord_list()?)),
        TAG_EDIT => {
            let n = r.u16()? as usize;
            let mut cells = Vec::with_capacity(n);
            for _ in 0..n {
                let cx = r.i32()?;
                let cy = r.i32()?;
                let lx = r.u8()?;
                let ly = r.u8()?;
                let alive = r.u8()? != 0;
                if (lx as usize) >= CHUNK_SIZE || (ly as usize) >= CHUNK_SIZE {
                    return Err(DecodeError::BadLocalCoord);
                }
                cells.push(EditCell { cx, cy, lx, ly, alive });
            }
            Ok(ClientMsg::Edit(cells))
        }
        TAG_CLAIM_CREATE => Ok(ClientMsg::ClaimCreate((r.i32()?, r.i32()?))),
        TAG_CLAIM_DELETE => Ok(ClientMsg::ClaimDelete),
        t => Err(DecodeError::UnknownTag(t)),
    }
}

fn write_chunk_payload(out: &mut Vec<u8>, cx: i32, cy: i32, tick: u64, bits: &[u8; BITS_BYTES]) {
    out.extend_from_slice(&cx.to_le_bytes());
    out.extend_from_slice(&cy.to_le_bytes());
    out.extend_from_slice(&tick.to_le_bytes());
    out.extend_from_slice(bits);
}

fn write_coord_list(out: &mut Vec<u8>, coords: &[(i32, i32)]) {
    let n = u16::try_from(coords.len()).expect("coord list >= 65536");
    out.extend_from_slice(&n.to_le_bytes());
    for (x, y) in coords {
        out.extend_from_slice(&x.to_le_bytes());
        out.extend_from_slice(&y.to_le_bytes());
    }
}

/// Pack `[u64; CHUNK_SIZE]` rows into the wire byte layout.
pub fn rows_to_bits(rows: &[u64; CHUNK_SIZE]) -> [u8; BITS_BYTES] {
    let mut out = [0u8; BITS_BYTES];
    for (y, row) in rows.iter().enumerate() {
        out[y * 8..y * 8 + 8].copy_from_slice(&row.to_le_bytes());
    }
    out
}

/// Inverse of [`rows_to_bits`].
pub fn bits_to_rows(bits: &[u8; BITS_BYTES]) -> [u64; CHUNK_SIZE] {
    let mut out = [0u64; CHUNK_SIZE];
    for (y, row) in out.iter_mut().enumerate() {
        let chunk: [u8; 8] = bits[y * 8..y * 8 + 8].try_into().unwrap();
        *row = u64::from_le_bytes(chunk);
    }
    out
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&[u8], DecodeError> {
        if self.pos == 0 && self.buf.is_empty() {
            return Err(DecodeError::Empty);
        }
        if self.pos + n > self.buf.len() {
            return Err(DecodeError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn f32(&mut self) -> Result<f32, DecodeError> {
        let s = self.take(4)?;
        Ok(f32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i32(&mut self) -> Result<i32, DecodeError> {
        Ok(self.u32()? as i32)
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        let s = self.take(8)?;
        Ok(u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
    }
    fn chunk_payload(&mut self) -> Result<(i32, i32, u64, [u8; BITS_BYTES]), DecodeError> {
        let cx = self.i32()?;
        let cy = self.i32()?;
        let tick = self.u64()?;
        let s = self.take(BITS_BYTES)?;
        let mut bits = [0u8; BITS_BYTES];
        bits.copy_from_slice(s);
        Ok((cx, cy, tick, bits))
    }
    fn i64(&mut self) -> Result<i64, DecodeError> {
        let s = self.take(8)?;
        Ok(i64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
    }
    fn coord_list(&mut self) -> Result<Vec<(i32, i32)>, DecodeError> {
        let n = self.u16()? as usize;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push((self.i32()?, self.i32()?));
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_bits() -> [u8; BITS_BYTES] {
        let mut b = [0u8; BITS_BYTES];
        for (i, v) in b.iter_mut().enumerate() {
            *v = (i & 0xff) as u8;
        }
        b
    }

    #[test]
    fn server_roundtrip_all_variants() {
        let bits = sample_bits();
        let cases = [
            ServerMsg::Hello { tick: 7, chunk_size: CHUNK_SIZE as u8 },
            ServerMsg::ChunkState { cx: -5, cy: 12, tick: 99, bits },
            ServerMsg::ChunkDelta { cx: i32::MIN, cy: i32::MAX, tick: u64::MAX, bits },
            ServerMsg::Regions { regions: Arc::from(vec![
                Region { x: -100, y: -50, w: 200, h: 100, flags: FLAG_FROZEN | FLAG_LOCKED, owner: 0 },
                Region { x: 0, y: 0, w: 32, h: 32, flags: FLAG_OWNED, owner: 42 },
            ]) },
            ServerMsg::Reaped { cx: 0, cy: -1 },
            ServerMsg::Stats { tick: 42, live_chunks: 1234, tick_rate_hz: 9.994, tick_utilization: 0.087 },
            ServerMsg::Sync { tick: u64::MAX },
            ServerMsg::EditApplied { cx: -3, cy: 7, cells: vec![
                EditCell { cx: -3, cy: 7, lx: 0, ly: 0, alive: true },
                EditCell { cx: -3, cy: 7, lx: 63, ly: 63, alive: false },
            ]},
            ServerMsg::EditApplied { cx: 0, cy: 0, cells: vec![] },
            ServerMsg::AuthState { uid: 42, claim: Some((-3, 7)), name: "Alice".into(), email: "a@b.com".into() },
            ServerMsg::AuthState { uid: 0, claim: None, name: String::new(), email: String::new() },
            ServerMsg::ClaimResult { ok: true },
            ServerMsg::ClaimResult { ok: false },
        ];
        for msg in &cases {
            let mut buf = Vec::new();
            encode_server(msg, &mut buf);
            let decoded = decode_server(&buf).expect("decode");
            assert_eq!(&decoded, msg);
        }
    }

    #[test]
    fn client_roundtrip_all_variants() {
        let cases = [
            ClientMsg::Subscribe(vec![(1, 2), (-3, -4)]),
            ClientMsg::Unsubscribe(vec![]),
            ClientMsg::Edit(vec![
                EditCell { cx: 1, cy: 2, lx: 0, ly: 0, alive: true },
                EditCell { cx: -1, cy: 0, lx: 31, ly: 31, alive: false },
            ]),
            ClientMsg::ClaimCreate((-5, 3)),
            ClientMsg::ClaimDelete,
        ];
        for msg in &cases {
            let mut buf = Vec::new();
            encode_client(msg, &mut buf);
            let decoded = decode_client(&buf).expect("decode");
            assert_eq!(&decoded, msg);
        }
    }

    #[test]
    fn rows_bits_roundtrip() {
        let mut rows = [0u64; CHUNK_SIZE];
        for (i, r) in rows.iter_mut().enumerate() {
            *r = 0xdead_beef_dead_0000 ^ (i as u64);
        }
        assert_eq!(bits_to_rows(&rows_to_bits(&rows)), rows);
    }

    #[test]
    fn rejects_unknown_tag() {
        assert!(matches!(decode_server(&[0xff]), Err(DecodeError::UnknownTag(0xff))));
        assert!(matches!(decode_client(&[0xff]), Err(DecodeError::UnknownTag(0xff))));
    }

    #[test]
    fn rejects_truncated() {
        let mut buf = Vec::new();
        encode_server(&ServerMsg::Stats { tick: 1, live_chunks: 0, tick_rate_hz: 0.0, tick_utilization: 0.0 }, &mut buf);
        buf.pop();
        assert!(matches!(decode_server(&buf), Err(DecodeError::Truncated)));
    }

    #[test]
    fn rejects_bad_local_coord() {
        let bad = ClientMsg::Edit(vec![EditCell { cx: 0, cy: 0, lx: CHUNK_SIZE as u8, ly: 0, alive: true }]);
        let mut buf = Vec::new();
        encode_client(&bad, &mut buf);
        assert!(matches!(decode_client(&buf), Err(DecodeError::BadLocalCoord)));
    }
}
