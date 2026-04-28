//! Simulation task. Owns the [`World`] and the tick loop. Detached from the hub /
//! networking - the only outputs are per-tick [`SimEvent`]s on a channel.
//!
//! Inbound: [`SimCmd`] from the hub (edits, reap requests).
//! Outbound: [`SimEvent`] for the hub; [`crate::io_task::IoCmd`] for the IO task
//! (WAL appends, snapshot serializations).

use crate::config::Config;
use crate::io_task::IoCmd;
use crate::metrics::Metrics;
use protocol::{rows_to_bits, EditCell, BITS_BYTES};
use simulation::{ChunkCoord, Detector, PromoteRequest, TickOutcome, World, CHUNK_SIZE_I64};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};

#[derive(Debug)]
pub enum SimCmd {
    Edit(Vec<EditCell>),
    Reap(Vec<ChunkCoord>),
    WakeIfPaused(Vec<ChunkCoord>),
}

#[derive(Debug, Clone)]
pub struct ChunkSnap {
    pub coord: ChunkCoord,
    pub bits: [u8; BITS_BYTES],
    /// Boxed because frozen chunks are rare; keeps the unfrozen `ChunkSnap`
    /// from carrying a 256-byte inline `Option` payload across the channel.
    pub frozen_mask: Option<Box<[u8; BITS_BYTES]>>,
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
/// Promote requests queued back from detector to sim.
const PROMOTE_CAPACITY: usize = 1024;

pub fn spawn(
    cfg: Config,
    world: World,
    io_tx: mpsc::Sender<IoCmd>,
    metrics: Arc<Metrics>,
) -> SimHandles {
    let (cmd_tx, cmd_rx) = mpsc::channel(1024);
    let (event_tx, event_rx) = mpsc::channel(SIM_EVENT_CAPACITY);
    let (promote_tx, promote_rx) = mpsc::channel::<PromoteRequest>(PROMOTE_CAPACITY);

    let detector = Arc::new(Mutex::new(Detector::new()));
    let interval_ms = cfg.oscillator_detection_interval_ms;
    let budget = cfg.oscillator_detection_max_chunks_per_step;
    tokio::spawn(detector_run(Arc::clone(&detector), promote_tx, interval_ms, budget));

    tokio::spawn(run(cfg, world, cmd_rx, event_tx, io_tx, detector, promote_rx, metrics));
    SimHandles { cmd_tx, event_rx }
}

async fn detector_run(
    detector: Arc<Mutex<Detector>>,
    promote_tx: mpsc::Sender<PromoteRequest>,
    interval_ms: u64,
    budget: usize,
) {
    let mut promote_buf: Vec<PromoteRequest> = Vec::with_capacity(budget);
    let mut scan = interval(Duration::from_millis(interval_ms.max(1)));
    scan.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        scan.tick().await;
        promote_buf.clear();
        {
            let mut det = detector.lock().expect("detector mutex poisoned");
            det.scan(budget, &mut promote_buf);
        }
        for req in promote_buf.drain(..) {
            if promote_tx.send(req).await.is_err() {
                return;
            }
        }
    }
}

async fn run(
    cfg: Config,
    mut world: World,
    mut cmd_rx: mpsc::Receiver<SimCmd>,
    event_tx: mpsc::Sender<SimEvent>,
    io_tx: mpsc::Sender<IoCmd>,
    detector: Arc<Mutex<Detector>>,
    mut promote_rx: mpsc::Receiver<PromoteRequest>,
    metrics: Arc<Metrics>,
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
                bits: rows_to_bits(chunk.rows()),
                frozen_mask: chunk.frozen.as_ref().map(|m| Box::new(rows_to_bits(&m.mask))),
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
    let mut last_overrun_warn: Option<Instant> = None;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return };
                match cmd {
                    SimCmd::Edit(cells) => {
                        let mut wakes = 0u64;
                        {
                            let mut det = detector.lock().expect("detector mutex poisoned");
                            for c in &cells {
                                if world.wake_if_paused((c.cx, c.cy)) {
                                    wakes += 1;
                                }
                                det.forget((c.cx, c.cy));
                            }
                        }
                        if wakes > 0 {
                            metrics.oscillator_wakes_total.inc_by(wakes);
                        }
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
                        let mut det = detector.lock().expect("detector mutex poisoned");
                        for coord in coords {
                            world.remove_chunk(coord);
                            det.forget(coord);
                        }
                    }
                    SimCmd::WakeIfPaused(coords) => {
                        let mut woken: HashSet<ChunkCoord> = HashSet::new();
                        for coord in coords {
                            if world.wake_if_paused(coord) {
                                woken.insert(coord);
                            }
                        }
                        if !woken.is_empty() {
                            metrics.oscillator_wakes_total.inc_by(woken.len() as u64);
                            let hint = woken.len();
                            let snaps = collect(&world, woken.into_iter(), hint);
                            let now_tick = world.tick_number();
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
                                .expect("sim event channel closed during wake (hub gone)");
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                let mut drained = 0usize;
                let mut promotions = 0u64;
                while drained < cfg.oscillator_promote_max_per_tick {
                    match promote_rx.try_recv() {
                        Ok(req) => {
                            if world.promote_oscillator(req.coord, req.period) {
                                promotions += 1;
                            }
                            drained += 1;
                        }
                        Err(_) => break,
                    }
                }
                if promotions > 0 {
                    metrics.oscillator_promotions_total.inc_by(promotions);
                }
                let start = Instant::now();
                world.tick_into(&mut outcome);
                if !outcome.hash_reports.is_empty() {
                    let tracked;
                    {
                        let mut det = detector.lock().expect("detector mutex poisoned");
                        for r in outcome.hash_reports.drain(..) {
                            det.observe(r);
                        }
                        tracked = det.len();
                    }
                    metrics.oscillator_tracked.set(tracked as i64);
                }
                metrics.oscillator_chunks.set(world.oscillator_count() as i64);
                if outcome.perturbation_wakes > 0 {
                    metrics.oscillator_wakes_total.inc_by(u64::from(outcome.perturbation_wakes));
                }
                let elapsed = start.elapsed();
                if elapsed > period {
                    let now_real = Instant::now();
                    let throttle = match last_overrun_warn {
                        Some(t) => now_real.duration_since(t) >= Duration::from_secs(1),
                        None => true,
                    };
                    if throttle {
                        last_overrun_warn = Some(now_real);
                        tracing::warn!(
                            tick = world.tick_number(),
                            compute_ms = elapsed.as_millis() as u64,
                            budget_ms = period.as_millis() as u64,
                            "tick overran budget; sim frequency dropping",
                        );
                    }
                }
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
                        compute_duration: elapsed,
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
                bits: rows_to_bits(chunk.rows()),
                frozen_mask: chunk.frozen.as_ref().map(|m| Box::new(rows_to_bits(&m.mask))),
                live_count: chunk.live_count(),
            });
        }
    }
    out
}
