//! Network/peer hub. Owns the peer registry, subscription index, and a passive chunk
//! mirror. Receives [`SimEvent`]s from the simulation task; routes [`SimCmd::Edit`] /
//! [`SimCmd::Reap`] back. Never touches the [`simulation::World`].

use crate::claims::ClaimManager;
use crate::config::Config;
use crate::metrics::Metrics;
use crate::reaper::{self, ReapInfo};
use crate::region;
use crate::sim::{SimCmd, SimEvent};
use bytes::Bytes;
use protocol::{encode_server, ClientMsg, Region, ServerMsg, BITS_BYTES};
use simulation::{ChunkCoord, CHUNK_SIZE, CHUNK_SIZE_I64};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

pub type PeerId = u64;
pub type Outbound = mpsc::Sender<Bytes>;

#[derive(Debug, Clone)]
pub struct PeerUser {
    pub uid: u32,
    pub email_key: String,
}

#[derive(Debug)]
pub enum HubCmd {
    Join { reply: oneshot::Sender<JoinAccepted> },
    Leave { peer_id: PeerId },
    Client { peer_id: PeerId, msg: ClientMsg },
    AuthPeer { peer_id: PeerId, user: PeerUser },
    ClaimCreate { peer_id: PeerId, coord: ChunkCoord },
    ClaimDelete { peer_id: PeerId },
}

#[derive(Debug)]
pub struct JoinAccepted {
    pub peer_id: PeerId,
    pub outbound: mpsc::Receiver<Bytes>,
    pub hello: ServerMsg,
}

struct Peer {
    tx: Outbound,
    subscribed: HashSet<ChunkCoord>,
}

struct MirrorEntry {
    bits: [u8; BITS_BYTES],
    /// Cached frozen mask for the chunk (1 bit/cell). Built from regions; used
    /// for `is_frozen` reaper input. None if no frozen region intersects.
    /// Boxed because most chunks are unfrozen; saves 256 B per mirror entry.
    frozen_mask: Option<Box<[u8; BITS_BYTES]>>,
    live_count: u32,
    tick: u64,
}

pub fn spawn(
    cfg: Config,
    sim_cmd_tx: mpsc::Sender<SimCmd>,
    sim_event_rx: mpsc::Receiver<SimEvent>,
    static_regions: Arc<[Region]>,
    claim_mgr: ClaimManager,
    metrics: Arc<Metrics>,
) -> mpsc::Sender<HubCmd> {
    let (tx, rx) = mpsc::channel(1024);
    let regions = claim_mgr.build_regions(&static_regions);
    let owned_chunk_set = claim_mgr.owned_chunk_set();
    let hub = Hub {
        config: cfg,
        sim_cmd_tx,
        peers: HashMap::new(),
        chunk_subs: HashMap::new(),
        last_seen_tick: HashMap::new(),
        mirror: HashMap::new(),
        static_regions,
        claim_mgr,
        peer_user: HashMap::new(),
        owned_chunk_set,
        regions,
        next_peer_id: 1,
        metrics,
        latest_tick: 0,
        window_started: Instant::now(),
        ticks_in_window: 0,
        compute_in_window: Duration::ZERO,
        recipients_scratch: Vec::new(),
        peer_ids_scratch: Vec::new(),
        reaper_subscribed_scratch: HashSet::new(),
        reaper_info_scratch: HashMap::new(),
        pending_sim_unsub: Vec::new(),
    };
    tokio::spawn(hub.run(rx, sim_event_rx));
    tx
}

struct Hub {
    config: Config,
    sim_cmd_tx: mpsc::Sender<SimCmd>,
    peers: HashMap<PeerId, Peer>,
    chunk_subs: HashMap<ChunkCoord, HashSet<PeerId>>,
    last_seen_tick: HashMap<ChunkCoord, u64>,
    mirror: HashMap<ChunkCoord, MirrorEntry>,
    /// Immutable static regions from regions.toml.
    static_regions: Arc<[Region]>,
    claim_mgr: ClaimManager,
    /// Per-peer authenticated user info.
    peer_user: HashMap<PeerId, PeerUser>,
    /// Chunk coords covered by any claim; used to bypass has_live_boundary opt.
    owned_chunk_set: HashSet<ChunkCoord>,
    /// Combined static + claim regions broadcast to clients.
    regions: Arc<[Region]>,
    next_peer_id: PeerId,
    metrics: Arc<Metrics>,
    latest_tick: u64,
    window_started: Instant,
    ticks_in_window: u32,
    compute_in_window: Duration,
    recipients_scratch: Vec<PeerId>,
    peer_ids_scratch: Vec<PeerId>,
    reaper_subscribed_scratch: HashSet<ChunkCoord>,
    reaper_info_scratch: HashMap<ChunkCoord, ReapInfo>,
    /// Coords whose subscriber count just hit zero. Buffered because they can
    /// originate from sync paths (a peer drop on backpressure during fan-out)
    /// where we cannot await; drained by the run loop after each event.
    pending_sim_unsub: Vec<ChunkCoord>,
}

impl Hub {
    async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<HubCmd>,
        mut sim_rx: mpsc::Receiver<SimEvent>,
    ) {
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(c) => self.handle_cmd(c).await,
                        None => return,
                    }
                }
                ev = sim_rx.recv() => {
                    match ev {
                        Some(e) => self.handle_sim_event(e).await,
                        None => return,
                    }
                }
            }
            self.flush_sim_unsub_pending().await;
        }
    }

    async fn handle_cmd(&mut self, cmd: HubCmd) {
        match cmd {
            HubCmd::Join { reply } => {
                let peer_id = self.next_peer_id;
                self.next_peer_id += 1;
                let (tx, rx) = mpsc::channel(self.config.peer_outbound_capacity);
                self.peers.insert(peer_id, Peer {
                    tx,
                    subscribed: HashSet::new(),
                });
                let hello = ServerMsg::Hello {
                    tick: self.latest_tick,
                    chunk_size: CHUNK_SIZE as u8,
                };
                if reply.send(JoinAccepted { peer_id, outbound: rx, hello }).is_err() {
                    // ws task vanished before we replied; tear down the slot we just made.
                    self.drop_peer(peer_id);
                    return;
                }
                // Send the region table right after Hello so the client can
                // materialise per-chunk flag overlays before any state arrives.
                let regions_msg = ServerMsg::Regions { regions: Arc::clone(&self.regions) };
                self.send_to(peer_id, &regions_msg);
                self.metrics.client_sessions.set(self.peers.len() as i64);
            }
            HubCmd::Leave { peer_id } => self.drop_peer(peer_id),
            HubCmd::Client { peer_id, msg } => self.handle_client_msg(peer_id, msg).await,
            HubCmd::AuthPeer { peer_id, user } => {
                self.peer_user.insert(peer_id, user);
            }
            HubCmd::ClaimCreate { peer_id, coord } => {
                self.handle_claim_create(peer_id, coord).await;
            }
            HubCmd::ClaimDelete { peer_id } => {
                self.handle_claim_delete(peer_id).await;
            }
        }
    }

    async fn handle_client_msg(&mut self, peer_id: PeerId, msg: ClientMsg) {
        match msg {
            ClientMsg::Subscribe(coords) => {
                let now = self.latest_tick;
                let cap = self.config.client_max_chunks as usize;
                let mut to_send: Vec<ServerMsg> = Vec::new();
                let mut sim_subscribe: Vec<ChunkCoord> = Vec::new();
                let mut overflow = false;
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    for coord in coords {
                        if peer.subscribed.contains(&coord) { continue; }
                        if peer.subscribed.len() >= cap {
                            overflow = true;
                            break;
                        }
                        peer.subscribed.insert(coord);
                        let entry = self.chunk_subs.entry(coord).or_default();
                        let was_zero = entry.is_empty();
                        entry.insert(peer_id);
                        if was_zero {
                            sim_subscribe.push(coord);
                        }
                        self.last_seen_tick.insert(coord, now);
                        if let Some(m) = self.mirror.get(&coord) {
                            if m.live_count > 0 || m.frozen_mask.is_some() {
                                to_send.push(ServerMsg::ChunkState {
                                    cx: coord.0,
                                    cy: coord.1,
                                    tick: m.tick,
                                    bits: m.bits,
                                });
                            }
                        }
                    }
                }
                if overflow {
                    tracing::warn!(peer = peer_id, cap, "subscribe exceeds client_max_chunks; dropping peer");
                    self.drop_peer(peer_id);
                    return;
                }
                if !sim_subscribe.is_empty() {
                    if let Err(e) = self.sim_cmd_tx.send(SimCmd::Subscribe(sim_subscribe)).await {
                        tracing::error!(err = %e, "sim cmd channel closed during subscribe");
                        return;
                    }
                }
                for msg in to_send {
                    if !self.send_to(peer_id, &msg) {
                        return;
                    }
                    self.metrics.chunk_state_sent_total.inc();
                }
            }
            ClientMsg::Unsubscribe(coords) => {
                let mut sim_unsubscribe: Vec<ChunkCoord> = Vec::new();
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    for coord in coords {
                        if peer.subscribed.remove(&coord) {
                            if let Some(set) = self.chunk_subs.get_mut(&coord) {
                                set.remove(&peer_id);
                                if set.is_empty() {
                                    self.chunk_subs.remove(&coord);
                                    sim_unsubscribe.push(coord);
                                }
                            }
                        }
                    }
                }
                if !sim_unsubscribe.is_empty() {
                    if let Err(e) = self.sim_cmd_tx.send(SimCmd::Unsubscribe(sim_unsubscribe)).await {
                        tracing::error!(err = %e, "sim cmd channel closed during unsubscribe");
                        return;
                    }
                }
            }
            ClientMsg::Edit(cells) => {
                let editor_uid = self.peer_user.get(&peer_id).map(|u| u.uid);
                let allowed: Vec<_> = cells.into_iter().filter(|c| {
                    let ax = i64::from(c.cx) * CHUNK_SIZE_I64 + i64::from(c.lx);
                    let ay = i64::from(c.cy) * CHUNK_SIZE_I64 + i64::from(c.ly);
                    region::can_edit(&self.regions, ax, ay, editor_uid)
                }).collect();
                if !allowed.is_empty() {
                    self.metrics.edits_total.inc_by(allowed.len() as u64);
                    self.sim_cmd_tx
                        .send(SimCmd::Edit(allowed))
                        .await
                        .expect("sim cmd channel closed (sim task gone)");
                }
            }
            // Intercepted by ws task before reaching hub; unreachable in practice.
            ClientMsg::ClaimCreate(_) | ClientMsg::ClaimDelete => {}
        }
    }

    async fn handle_sim_event(&mut self, ev: SimEvent) {
        let fanout_start = Instant::now();
        let now = ev.tick;
        self.latest_tick = now;
        let bytes_at_start = self.metrics.bytes_sent_total.get();
        let msgs_at_start = self.metrics.messages_sent_total.get();

        if ev.is_tick {
            for coord in self.chunk_subs.keys() {
                self.last_seen_tick.insert(*coord, now);
            }
        }

        for &coord in &ev.removed {
            self.mirror.remove(&coord);
            self.last_seen_tick.remove(&coord);
            self.broadcast(&ServerMsg::Reaped { cx: coord.0, cy: coord.1 });
        }

        // Update mirror for all changed chunks; send ChunkDelta for those with live
        // boundary cells. Boundary-active chunks interact with external neighbors,
        // so the client cannot simulate them correctly from cached state alone.
        // Isolated chunks (no live boundary cells) are simulated correctly by the
        // client's local GoL without any cross-chunk data.
        for snap in ev.changed {
            let coord = snap.coord;
            let bits = snap.bits;
            self.mirror.insert(coord, MirrorEntry {
                bits,
                frozen_mask: snap.frozen_mask,
                live_count: snap.live_count,
                tick: now,
            });

            if ev.initial || (!has_live_boundary(&bits) && !self.owned_chunk_set.contains(&coord)) { continue; }

            self.recipients_scratch.clear();
            match self.chunk_subs.get(&coord) {
                Some(subs) if !subs.is_empty() => {
                    self.recipients_scratch.extend(subs.iter().copied());
                }
                _ => continue,
            };
            let bytes = self.encode_msg(&ServerMsg::ChunkDelta {
                cx: coord.0,
                cy: coord.1,
                tick: now,
                bits,
            });
            for i in 0..self.recipients_scratch.len() {
                let pid = self.recipients_scratch[i];
                if self.send_bytes(pid, bytes.clone()) {
                    self.metrics.chunk_delta_sent_total.inc();
                }
            }
        }

        // Broadcast edit payloads before Sync so clients patch local state
        // before their GoL step.
        if !ev.initial {
            for (coord, cells) in ev.edits {
                self.recipients_scratch.clear();
                match self.chunk_subs.get(&coord) {
                    Some(subs) if !subs.is_empty() => {
                        self.recipients_scratch.extend(subs.iter().copied());
                    }
                    _ => continue,
                };
                let bytes = self.encode_msg(&ServerMsg::EditApplied {
                    cx: coord.0,
                    cy: coord.1,
                    cells,
                });
                for i in 0..self.recipients_scratch.len() {
                    let pid = self.recipients_scratch[i];
                    if self.send_bytes(pid, bytes.clone()) {
                        self.metrics.chunk_delta_sent_total.inc();
                    }
                }
            }

            // Sync drives the client GoL step; sent after edits so clients have
            // the patched state before advancing.
            if ev.is_tick {
                self.broadcast(&ServerMsg::Sync { tick: now });
            }
        }

        // Reaper. Skip on the initial dump (which has no removed-since-last context).
        if ev.is_tick && self.mirror.len() > self.config.max_live_chunks {
            self.reaper_subscribed_scratch.clear();
            for peer in self.peers.values() {
                self.reaper_subscribed_scratch.extend(peer.subscribed.iter().copied());
            }
            self.reaper_info_scratch.clear();
            self.reaper_info_scratch.reserve(self.mirror.len());
            for (&coord, m) in &self.mirror {
                self.reaper_info_scratch.insert(coord, ReapInfo {
                    live_count: m.live_count,
                    is_frozen: m.frozen_mask.is_some(),
                });
            }
            let to_reap = reaper::pick_reapable(
                &self.reaper_info_scratch,
                &self.reaper_subscribed_scratch,
                &self.last_seen_tick,
                now,
                self.config.max_live_chunks,
            );
            if !to_reap.is_empty() {
                self.metrics.reaped_chunks_total.inc_by(to_reap.len() as u64);
                // Broadcast Reaped to all peers and update mirror immediately so the
                // next tick's reaper input is consistent. Sim asynchronously removes.
                for &coord in &to_reap {
                    self.mirror.remove(&coord);
                    self.last_seen_tick.remove(&coord);
                    self.broadcast(&ServerMsg::Reaped { cx: coord.0, cy: coord.1 });
                }
                self.sim_cmd_tx
                    .send(SimCmd::Reap(to_reap))
                    .await
                    .expect("sim cmd channel closed during reap (sim task gone)");
            }
        }

        // Sliding-window rate/util counters only advance on true ticks.
        if ev.is_tick {
            self.compute_in_window += ev.compute_duration;
            self.ticks_in_window += 1;
            self.metrics
                .tick_compute_seconds
                .observe(ev.compute_duration.as_secs_f64());
        }
        let window = self.window_started.elapsed();
        if window >= Duration::from_secs(1) {
            let secs = window.as_secs_f64();
            let hz = f64::from(self.ticks_in_window) / secs;
            self.metrics.tick_rate_hz.set(hz);
            let budget = Duration::from_micros(1_000_000 / u64::from(self.config.tick_hz)) * self.ticks_in_window;
            let util = if budget.is_zero() { 0.0 } else {
                self.compute_in_window.as_secs_f64() / budget.as_secs_f64()
            };
            self.metrics.tick_utilization.set(util);
            self.window_started = Instant::now();
            self.ticks_in_window = 0;
            self.compute_in_window = Duration::ZERO;
        }
        self.metrics.live_chunks.set(ev.live_count as i64);

        if ev.is_tick && !self.peers.is_empty() {
            let cap = self.config.peer_outbound_capacity;
            let max_depth = self.peers.values()
                .map(|p| cap - p.tx.capacity())
                .max()
                .unwrap_or(0);
            self.metrics.peer_outbound_depth_max.observe(max_depth as f64);
        }

        let heartbeat_every = u64::from(self.config.tick_hz);
        if ev.is_tick && heartbeat_every > 0 && now % heartbeat_every == 0 {
            let live_chunks = u32::try_from(self.metrics.live_chunks.get())
                .expect("live_chunks exceeds u32 (>4B chunks)");
            let stats = ServerMsg::Stats {
                tick: now,
                live_chunks,
                tick_rate_hz: self.metrics.tick_rate_hz.get() as f32,
                tick_utilization: self.metrics.tick_utilization.get() as f32,
            };
            self.broadcast(&stats);
        }

        if ev.is_tick {
            let bytes_delta = self.metrics.bytes_sent_total.get().saturating_sub(bytes_at_start);
            let msgs_delta = self.metrics.messages_sent_total.get().saturating_sub(msgs_at_start);
            self.metrics.broadcast_bytes_per_tick.observe(bytes_delta as f64);
            self.metrics.broadcast_messages_per_tick.observe(msgs_delta as f64);
            self.metrics.hub_fanout_seconds.observe(fanout_start.elapsed().as_secs_f64());
        }
    }

    fn broadcast(&mut self, msg: &ServerMsg) {
        let bytes = self.encode_msg(msg);
        self.peer_ids_scratch.clear();
        self.peer_ids_scratch.extend(self.peers.keys().copied());
        for i in 0..self.peer_ids_scratch.len() {
            let pid = self.peer_ids_scratch[i];
            self.send_bytes(pid, bytes.clone());
        }
    }

    fn send_to(&mut self, peer_id: PeerId, msg: &ServerMsg) -> bool {
        let bytes = self.encode_msg(msg);
        self.send_bytes(peer_id, bytes)
    }

    fn encode_msg(&mut self, msg: &ServerMsg) -> Bytes {
        encode_once(msg)
    }

    fn send_bytes(&mut self, peer_id: PeerId, bytes: Bytes) -> bool {
        let len = bytes.len() as u64;
        let drop = match self.peers.get(&peer_id) {
            Some(peer) => peer.tx.try_send(bytes).is_err(),
            None => return false,
        };
        if drop {
            self.metrics.peer_dropped_backpressure_total.inc();
            tracing::warn!(
                peer = peer_id,
                cap = self.config.peer_outbound_capacity,
                "dropping peer: outbound queue full (backpressure)"
            );
            self.drop_peer(peer_id);
            false
        } else {
            self.metrics.bytes_sent_total.inc_by(len);
            self.metrics.messages_sent_total.inc();
            true
        }
    }

    fn drop_peer(&mut self, peer_id: PeerId) {
        if let Some(peer) = self.peers.remove(&peer_id) {
            for coord in peer.subscribed {
                if let Some(set) = self.chunk_subs.get_mut(&coord) {
                    set.remove(&peer_id);
                    if set.is_empty() {
                        self.chunk_subs.remove(&coord);
                        self.pending_sim_unsub.push(coord);
                    }
                }
            }
        }
        self.peer_user.remove(&peer_id);
        self.metrics.client_sessions.set(self.peers.len() as i64);
    }

    async fn flush_sim_unsub_pending(&mut self) {
        if self.pending_sim_unsub.is_empty() { return; }
        let coords = std::mem::take(&mut self.pending_sim_unsub);
        if let Err(e) = self.sim_cmd_tx.send(SimCmd::Unsubscribe(coords)).await {
            tracing::error!(err = %e, "sim cmd channel closed during pending unsubscribe flush");
        }
    }

    /// Rebuild region list + owned sets, notify sim, broadcast TAG_REGIONS to all.
    async fn apply_claims_update(&mut self) {
        self.regions = self.claim_mgr.build_regions(&self.static_regions);
        self.owned_chunk_set = self.claim_mgr.owned_chunk_set();
        let owned = self.claim_mgr.owned_coord_map();
        if let Err(e) = self.sim_cmd_tx.send(SimCmd::SetOwned(owned)).await {
            tracing::error!(err = %e, "sim cmd closed during SetOwned");
        }
        let msg = ServerMsg::Regions { regions: Arc::clone(&self.regions) };
        self.broadcast(&msg);
    }

    fn auth_state_for(&self, peer_id: PeerId) -> ServerMsg {
        let Some(user) = self.peer_user.get(&peer_id) else {
            return ServerMsg::AuthState { uid: 0, claim: None, name: String::new(), email: String::new(), providers: Vec::new() };
        };
        ServerMsg::AuthState {
            uid: user.uid,
            claim: self.claim_mgr.find_for_user(user.uid),
            name: String::new(),
            email: String::new(),
            providers: Vec::new(),
        }
    }

    async fn handle_claim_create(&mut self, peer_id: PeerId, coord: ChunkCoord) {
        let Some(user) = self.peer_user.get(&peer_id).cloned() else {
            self.send_to(peer_id, &ServerMsg::ClaimResult { ok: false });
            return;
        };
        let (cx, cy) = coord;
        if !self.claim_mgr.try_create(user.uid, cx, cy, &self.regions) {
            self.send_to(peer_id, &ServerMsg::ClaimResult { ok: false });
            return;
        }
        self.claim_mgr.persist_create(&user.email_key, user.uid, cx, cy);
        self.apply_claims_update().await;
        self.send_to(peer_id, &ServerMsg::ClaimResult { ok: true });
        let state = self.auth_state_for(peer_id);
        self.send_to(peer_id, &state);
    }

    async fn handle_claim_delete(&mut self, peer_id: PeerId) {
        let Some(user) = self.peer_user.get(&peer_id).cloned() else {
            self.send_to(peer_id, &ServerMsg::ClaimResult { ok: false });
            return;
        };
        if !self.claim_mgr.try_delete(user.uid) {
            self.send_to(peer_id, &ServerMsg::ClaimResult { ok: false });
            return;
        }
        self.claim_mgr.persist_delete(&user.email_key);
        self.apply_claims_update().await;
        self.send_to(peer_id, &ServerMsg::ClaimResult { ok: true });
        let state = self.auth_state_for(peer_id);
        self.send_to(peer_id, &state);
    }
}

fn encode_once(msg: &ServerMsg) -> Bytes {
    let mut buf = Vec::with_capacity(encode_capacity(msg));
    encode_server(msg, &mut buf);
    Bytes::from(buf)
}

fn encode_capacity(msg: &ServerMsg) -> usize {
    match msg {
        ServerMsg::ChunkState { .. } | ServerMsg::ChunkDelta { .. } => BITS_BYTES + 1 + 4 + 4 + 8,
        ServerMsg::Hello { .. } => 1 + 8 + 1,
        ServerMsg::Reaped { .. } => 1 + 4 + 4,
        ServerMsg::Stats { .. } => 1 + 8 + 4 + 4 + 4,
        ServerMsg::Regions { regions } => 1 + 2 + regions.len() * (8 + 8 + 4 + 4 + 1 + 4),
        ServerMsg::Sync { .. } => 1 + 8,
        ServerMsg::EditApplied { cells, .. } => 1 + 4 + 4 + 2 + cells.len() * 3,
        ServerMsg::AuthState { name, email, providers, .. } =>
            1 + 4 + 1 + 4 + 4 + 2 + name.len() + 2 + email.len()
            + 1 + providers.iter().map(|(s, d)| 1 + s.len() + 1 + d.len()).sum::<usize>(),
        ServerMsg::ClaimResult { .. } => 1 + 1,
    }
}

/// True if any cell in the top row, bottom row, left column, or right column is alive.
/// Chunks with live boundary cells interact with their neighbors across chunk edges;
/// clients cannot simulate these correctly without external neighbor data, so the
/// server must send authoritative `ChunkDelta`s for them.
fn has_live_boundary(bits: &[u8; BITS_BYTES]) -> bool {
    // Top row (y=0): bytes 0..8
    if bits[..8].iter().any(|&b| b != 0) { return true; }
    // Bottom row (y=63): bytes 504..512 (CHUNK_SIZE-1)*8 = 504
    if bits[504..].iter().any(|&b| b != 0) { return true; }
    // Left column (x=0): LSB of byte at y*8; right column (x=63): MSB of byte at y*8+7.
    // Rows 0 and 63 already checked above; check rows 1..62.
    for y in 1..CHUNK_SIZE - 1 {
        let off = y * 8;
        if (bits[off] & 0x01) | (bits[off + 7] & 0x80) != 0 {
            return true;
        }
    }
    false
}

pub async fn join(tx: &mpsc::Sender<HubCmd>) -> Option<JoinAccepted> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(HubCmd::Join { reply: reply_tx }).await.ok()?;
    reply_rx.await.ok()
}
