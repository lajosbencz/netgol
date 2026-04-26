//! Simulation task. Owns the [`World`] and the tick loop. Detached from the hub /
//! networking - the only outputs are per-tick [`SimEvent`]s on a channel.
//!
//! Inbound: [`SimCmd`] from the hub (edits, reap requests).
//! Outbound: [`SimEvent`] for the hub; [`crate::io_task::IoCmd`] for the IO task
//! (WAL appends, snapshot serializations).

use crate::config::Config;
use crate::io_task::IoCmd;
use protocol::{rows_to_bits, EditCell, BITS_BYTES};
use simulation::{ChunkCoord, TickOutcome, World, CHUNK_SIZE_I64};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};

#[derive(Debug)]
pub enum SimCmd {
    Edit(Vec<EditCell>),
    Reap(Vec<ChunkCoord>),
}

#[derive(Debug, Clone)]
pub struct ChunkSnap {
    pub coord: ChunkCoord,
    pub bits: [u8; BITS_BYTES],
    pub frozen_mask: Option<[u8; BITS_BYTES]>,
    pub live_count: u32,
}

#[derive(Debug)]
pub struct SimEvent {
    pub tick: u64,
    /// Chunks that changed (or were touched by an edit) this turn, with current state.
    pub changed: Vec<ChunkSnap>,
    /// Chunks removed (natural emptying or reaper). Hub broadcasts `Reaped` for these.
    pub removed: Vec<ChunkCoord>,
    pub live_count: usize,
    pub compute_duration: Duration,
    /// True only on the very first event (full world dump after boot).
    pub initial: bool,
}

pub struct SimHandles {
    pub cmd_tx: mpsc::Sender<SimCmd>,
    pub event_rx: mpsc::Receiver<SimEvent>,
}

/// Bounded so a stalled hub fails loud (channel full → sim panics) instead of
/// quietly accumulating events forever. 256 slots = ~25s of headroom at 10 Hz.
const SIM_EVENT_CAPACITY: usize = 256;

pub fn spawn(cfg: Config, world: World, io_tx: mpsc::Sender<IoCmd>) -> SimHandles {
    let (cmd_tx, cmd_rx) = mpsc::channel(1024);
    let (event_tx, event_rx) = mpsc::channel(SIM_EVENT_CAPACITY);
    tokio::spawn(run(cfg, world, cmd_rx, event_tx, io_tx));
    SimHandles { cmd_tx, event_rx }
}

async fn run(
    cfg: Config,
    mut world: World,
    mut cmd_rx: mpsc::Receiver<SimCmd>,
    event_tx: mpsc::Sender<SimEvent>,
    io_tx: mpsc::Sender<IoCmd>,
) {
    let snapshot_interval_ticks = cfg.snapshot_interval_ticks;
    let mut next_snapshot_tick = world.tick_number() + snapshot_interval_ticks;

    // Initial dump so the hub can populate its mirror with all currently-loaded chunks
    // (boot snapshot + frozen regions).
    {
        let mut snaps: Vec<ChunkSnap> = Vec::with_capacity(world.len());
        for (coord, chunk) in world.iter_chunks() {
            snaps.push(ChunkSnap {
                coord,
                bits: rows_to_bits(&chunk.rows),
                frozen_mask: chunk.frozen.as_ref().map(|m| rows_to_bits(&m.mask)),
                live_count: chunk.live_count(),
            });
        }
        event_tx
            .send(SimEvent {
                tick: world.tick_number(),
                changed: snaps,
                removed: vec![],
                live_count: world.len(),
                compute_duration: Duration::ZERO,
                initial: true,
            })
            .await
            .expect("sim event channel closed before initial dump (hub gone)");
    }

    let period = Duration::from_micros(1_000_000 / u64::from(cfg.tick_hz));
    let mut ticker = interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut edit_touched: HashSet<ChunkCoord> = HashSet::new();
    let mut outcome = TickOutcome::default();
    let mut dedup_scratch: HashSet<ChunkCoord> = HashSet::new();

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return };
                match cmd {
                    SimCmd::Edit(cells) => {
                        for c in &cells {
                            let ax = i64::from(c.cx) * CHUNK_SIZE_I64 + i64::from(c.lx);
                            let ay = i64::from(c.cy) * CHUNK_SIZE_I64 + i64::from(c.ly);
                            world.set_cell(ax, ay, c.alive);
                            edit_touched.insert((c.cx, c.cy));
                        }
                        // Durable record before client visibility: WAL append + fsync.
                        // Sim doesn't block on the fsync - the IO task does it - so
                        // the only backpressure surfaces if the IO channel fills,
                        // which panics (see `IO_CMD_CAPACITY`).
                        let now_tick = world.tick_number();
                        io_tx
                            .send(IoCmd::AppendEdits { tick: now_tick, cells })
                            .await
                            .expect("io channel closed during edit append");
                        // Push immediate event so still-life edits surface on the
                        // client without waiting for the next tick.
                        if !edit_touched.is_empty() {
                            let hint = edit_touched.len();
                            let snaps = collect(&world, edit_touched.drain(), hint);
                            event_tx
                                .send(SimEvent {
                                    tick: now_tick,
                                    changed: snaps,
                                    removed: vec![],
                                    live_count: world.len(),
                                    compute_duration: Duration::ZERO,
                                    initial: false,
                                })
                                .await
                                .expect("sim event channel closed during edit (hub gone)");
                        }
                    }
                    SimCmd::Reap(coords) => {
                        // Hub already broadcast `Reaped` to peers before sending
                        // this; no SimEvent needed.
                        for coord in coords {
                            world.remove_chunk(coord);
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                let start = Instant::now();
                world.tick_into(&mut outcome);
                if !edit_touched.is_empty() {
                    dedup_scratch.clear();
                    dedup_scratch.extend(outcome.changed.iter().copied());
                    for c in edit_touched.drain() {
                        if dedup_scratch.insert(c) {
                            outcome.changed.push(c);
                        }
                    }
                }
                let hint = outcome.changed.len();
                let changed = collect(&world, outcome.changed.iter().copied(), hint);
                let removed: Vec<ChunkCoord> = outcome.removed.drain(..).collect();
                let now = world.tick_number();

                event_tx
                    .send(SimEvent {
                        tick: now,
                        changed,
                        removed,
                        live_count: world.len(),
                        compute_duration: start.elapsed(),
                        initial: false,
                    })
                    .await
                    .expect("sim event channel closed during tick (hub gone)");

                if now >= next_snapshot_tick {
                    next_snapshot_tick = now + snapshot_interval_ticks;
                    let bytes = crate::snapshot::serialize(&world);
                    io_tx
                        .send(IoCmd::Snapshot { tick: now, bytes })
                        .await
                        .expect("io channel closed during snapshot dispatch");
                }
            }
        }
    }
}

fn collect(world: &World, coords: impl Iterator<Item = ChunkCoord>, hint: usize) -> Vec<ChunkSnap> {
    let mut out = Vec::with_capacity(hint);
    for coord in coords {
        if let Some(chunk) = world.get_chunk(coord.0, coord.1) {
            out.push(ChunkSnap {
                coord,
                bits: rows_to_bits(&chunk.rows),
                frozen_mask: chunk.frozen.as_ref().map(|m| rows_to_bits(&m.mask)),
                live_count: chunk.live_count(),
            });
        }
    }
    out
}
