//! Network/peer hub. Owns the peer registry, subscription index, and a passive chunk
//! mirror. Receives [`SimEvent`]s from the simulation task; routes [`SimCmd::Edit`] /
//! [`SimCmd::Reap`] back. Never touches the [`simulation::World`].

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

#[derive(Debug)]
pub enum HubCmd {
    Join { reply: oneshot::Sender<JoinAccepted> },
    Leave { peer_id: PeerId },
    Client { peer_id: PeerId, msg: ClientMsg },
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
    frozen_mask: Option<[u8; BITS_BYTES]>,
    live_count: u32,
    tick: u64,
}

pub fn spawn(
    cfg: Config,
    sim_cmd_tx: mpsc::Sender<SimCmd>,
    sim_event_rx: mpsc::Receiver<SimEvent>,
    regions: Arc<[Region]>,
    metrics: Arc<Metrics>,
) -> mpsc::Sender<HubCmd> {
    let (tx, rx) = mpsc::channel(1024);
    let hub = Hub {
        config: cfg,
        sim_cmd_tx,
        peers: HashMap::new(),
        chunk_subs: HashMap::new(),
        last_seen_tick: HashMap::new(),
        mirror: HashMap::new(),
        regions,
        next_peer_id: 1,
        metrics,
        latest_tick: 0,
        window_started: Instant::now(),
        ticks_in_window: 0,
        compute_in_window: Duration::ZERO,
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
    regions: Arc<[Region]>,
    next_peer_id: PeerId,
    metrics: Arc<Metrics>,
    latest_tick: u64,
    window_started: Instant,
    ticks_in_window: u32,
    compute_in_window: Duration,
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
                let regions_msg = ServerMsg::Regions { regions: self.regions.to_vec() };
                self.send_to(peer_id, &regions_msg);
                self.metrics.client_sessions.set(self.peers.len() as i64);
            }
            HubCmd::Leave { peer_id } => self.drop_peer(peer_id),
            HubCmd::Client { peer_id, msg } => self.handle_client_msg(peer_id, msg).await,
        }
    }

    async fn handle_client_msg(&mut self, peer_id: PeerId, msg: ClientMsg) {
        match msg {
            ClientMsg::Subscribe(coords) => {
                let now = self.latest_tick;
                let cap = self.config.client_max_chunks as usize;
                let mut to_send: Vec<ServerMsg> = Vec::new();
                let mut overflow = false;
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    for coord in coords {
                        if peer.subscribed.contains(&coord) { continue; }
                        if peer.subscribed.len() >= cap {
                            overflow = true;
                            break;
                        }
                        peer.subscribed.insert(coord);
                        self.chunk_subs.entry(coord).or_default().insert(peer_id);
                        self.last_seen_tick.insert(coord, now);
                        // Adaptive: only send ChunkState if the chunk has live cells
                        // OR overlaps a frozen region. Empty unflagged chunks are
                        // visually empty.
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
                for msg in to_send {
                    if !self.send_to(peer_id, &msg) {
                        return;
                    }
                    self.metrics.chunk_state_sent_total.inc();
                }
            }
            ClientMsg::Unsubscribe(coords) => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    for coord in coords {
                        if peer.subscribed.remove(&coord) {
                            if let Some(set) = self.chunk_subs.get_mut(&coord) {
                                set.remove(&peer_id);
                                if set.is_empty() {
                                    self.chunk_subs.remove(&coord);
                                }
                            }
                        }
                    }
                }
            }
            ClientMsg::Edit(cells) => {
                // Filter out cells inside locked regions before forwarding.
                let allowed: Vec<_> = cells.into_iter().filter(|c| {
                    let ax = i64::from(c.cx) * CHUNK_SIZE_I64 + i64::from(c.lx);
                    let ay = i64::from(c.cy) * CHUNK_SIZE_I64 + i64::from(c.ly);
                    !region::is_locked(&self.regions, ax, ay)
                }).collect();
                if !allowed.is_empty() {
                    self.metrics.edits_total.inc_by(allowed.len() as u64);
                    self.sim_cmd_tx
                        .send(SimCmd::Edit(allowed))
                        .await
                        .expect("sim cmd channel closed (sim task gone)");
                }
            }
        }
    }

    async fn handle_sim_event(&mut self, ev: SimEvent) {
        let now = ev.tick;
        self.latest_tick = now;
        let bytes_at_start = self.metrics.bytes_sent_total.get();
        let msgs_at_start = self.metrics.messages_sent_total.get();

        if !ev.initial {
            for coord in self.chunk_subs.keys() {
                self.last_seen_tick.insert(*coord, now);
            }
        }

        for &coord in &ev.removed {
            self.mirror.remove(&coord);
            self.last_seen_tick.remove(&coord);
            self.broadcast(&ServerMsg::Reaped { cx: coord.0, cy: coord.1 });
        }

        for snap in &ev.changed {
            self.mirror.insert(snap.coord, MirrorEntry {
                bits: snap.bits,
                frozen_mask: snap.frozen_mask,
                live_count: snap.live_count,
                tick: now,
            });

            // Don't fanout the initial bulk dump - peers receive their slice on subscribe.
            if ev.initial { continue; }

            let recipients: Vec<PeerId> = match self.chunk_subs.get(&snap.coord) {
                Some(subs) if !subs.is_empty() => subs.iter().copied().collect(),
                _ => continue,
            };
            let bytes = encode_once(&ServerMsg::ChunkDelta {
                cx: snap.coord.0,
                cy: snap.coord.1,
                tick: now,
                bits: snap.bits,
            });
            for pid in recipients {
                if self.send_bytes(pid, bytes.clone()) {
                    self.metrics.chunk_delta_sent_total.inc();
                }
            }
        }

        // Reaper. Skip on the initial dump (which has no removed-since-last context).
        if !ev.initial && self.mirror.len() > self.config.max_live_chunks {
            let mut subscribed_set: HashSet<ChunkCoord> = HashSet::new();
            for peer in self.peers.values() {
                subscribed_set.extend(peer.subscribed.iter().copied());
            }
            let mut info_map: HashMap<ChunkCoord, ReapInfo> = HashMap::with_capacity(self.mirror.len());
            for (&coord, m) in &self.mirror {
                info_map.insert(coord, ReapInfo {
                    live_count: m.live_count,
                    is_frozen: m.frozen_mask.is_some(),
                });
            }
            let to_reap = reaper::pick_reapable(
                &info_map,
                &subscribed_set,
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
        if !ev.initial {
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

        // Per-tick fan-out summary: max queue depth across peers.
        if !ev.initial && !self.peers.is_empty() {
            let cap = self.config.peer_outbound_capacity;
            let max_depth = self.peers.values()
                .map(|p| cap - p.tx.capacity())
                .max()
                .unwrap_or(0);
            self.metrics.peer_outbound_depth_max.observe(max_depth as f64);
        }

        let heartbeat_every = u64::from(self.config.tick_hz);
        if !ev.initial && heartbeat_every > 0 && now % heartbeat_every == 0 {
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

        if !ev.initial {
            let bytes_delta = self.metrics.bytes_sent_total.get().saturating_sub(bytes_at_start);
            let msgs_delta = self.metrics.messages_sent_total.get().saturating_sub(msgs_at_start);
            self.metrics.broadcast_bytes_per_tick.observe(bytes_delta as f64);
            self.metrics.broadcast_messages_per_tick.observe(msgs_delta as f64);
        }
    }

    fn broadcast(&mut self, msg: &ServerMsg) {
        let bytes = encode_once(msg);
        let peer_ids: Vec<PeerId> = self.peers.keys().copied().collect();
        for pid in peer_ids {
            self.send_bytes(pid, bytes.clone());
        }
    }

    fn send_to(&mut self, peer_id: PeerId, msg: &ServerMsg) -> bool {
        self.send_bytes(peer_id, encode_once(msg))
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
                    }
                }
            }
        }
        self.metrics.client_sessions.set(self.peers.len() as i64);
    }
}

fn encode_once(msg: &ServerMsg) -> Bytes {
    let mut buf = Vec::new();
    encode_server(msg, &mut buf);
    Bytes::from(buf)
}

pub async fn join(tx: &mpsc::Sender<HubCmd>) -> Option<JoinAccepted> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(HubCmd::Join { reply: reply_tx }).await.ok()?;
    reply_rx.await.ok()
}
