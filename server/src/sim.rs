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
use simulation::{ChunkCoord, CoordMap, Detector, PromoteRequest, TickOutcome, World, CHUNK_SIZE_I64};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};

#[derive(Debug)]
pub enum SimCmd {
    Edit(Vec<EditCell>),
    Reap(Vec<ChunkCoord>),
    /// Coords newly observed by at least one peer (0->1 transitions). The sim
    /// wakes any paused entry and refuses to pause these coords while they
    /// remain in the subscribed set.
    Subscribe(Vec<ChunkCoord>),
    /// Coords no longer observed by any peer (1->0 transitions). Pausing
    /// becomes eligible again on the next detector promotion.
    Unsubscribe(Vec<ChunkCoord>),
    /// Replace the owned-chunk set. Owned chunks reject inbound edges from
    /// non-owned neighbours, preventing external births into claimed regions.
    SetOwned(CoordMap<()>),
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
    /// Chunks that changed this turn (natural GoL or post-edit), with current state.
    /// Used by the hub to keep its mirror up to date. Natural-evolution chunks are
    /// not broadcast to clients; edit chunks are covered by `edits`.
    pub changed: Vec<ChunkSnap>,
    /// Per-chunk edit payloads from `SimCmd::Edit` received before this event.
    /// Hub broadcasts `EditApplied` for each entry. Empty on tick events and the
    /// initial dump.
    pub edits: Vec<(ChunkCoord, Vec<EditCell>)>,
    /// Chunks removed (natural emptying or reaper). Hub broadcasts `Reaped` for these.
    pub removed: Vec<ChunkCoord>,
    pub live_count: usize,
    pub compute_duration: Duration,
    /// True only on the very first event (full world dump after boot).
    pub initial: bool,
    /// True when this event represents a completed GoL tick (vs an immediate
    /// edit notification). Hub sends `Sync` only on tick events.
    pub is_tick: bool,
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
                edits: vec![],
                removed: vec![],
                live_count: world.len(),
                compute_duration: Duration::ZERO,
                initial: true,
                is_tick: false,
            })
            .await
            .expect("sim event channel closed before initial dump (hub gone)");
    }

    let period = Duration::from_micros(1_000_000 / u64::from(cfg.tick_hz));
    let mut ticker = interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut outcome = TickOutcome::default();
    let mut last_overrun_warn: Option<Instant> = None;
    // Subscribed: coords with >=1 peer observing. Pause-ineligible while present.
    let mut subscribed: HashSet<ChunkCoord> = HashSet::new();

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
                        // Apply cells and group by chunk for EditApplied broadcast.
                        let mut cells_by_chunk: HashMap<ChunkCoord, Vec<EditCell>> = HashMap::new();
                        for c in &cells {
                            let ax = i64::from(c.cx) * CHUNK_SIZE_I64 + i64::from(c.lx);
                            let ay = i64::from(c.cy) * CHUNK_SIZE_I64 + i64::from(c.ly);
                            world.set_cell(ax, ay, c.alive);
                            cells_by_chunk.entry((c.cx, c.cy)).or_default().push(*c);
                        }
                        // Durable record before client visibility: WAL append + fsync.
                        let now_tick = world.tick_number();
                        io_tx
                            .send(IoCmd::AppendEdits { tick: now_tick, cells })
                            .await
                            .expect("io channel closed during edit append");
                        // Immediate event: lets hub broadcast EditApplied for all
                        // edited chunks. Also updates the mirror so new subscribers
                        // on still-life chunks see the current state.
                        let hint = cells_by_chunk.len();
                        let snaps = collect(&world, cells_by_chunk.keys().copied(), hint);
                        let edits: Vec<(ChunkCoord, Vec<EditCell>)> =
                            cells_by_chunk.into_iter().collect();
                        event_tx
                            .send(SimEvent {
                                tick: now_tick,
                                changed: snaps,
                                edits,
                                removed: vec![],
                                live_count: world.len(),
                                compute_duration: Duration::ZERO,
                                initial: false,
                                is_tick: false,
                            })
                            .await
                            .expect("sim event channel closed during edit (hub gone)");
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
                    SimCmd::Subscribe(coords) => {
                        let mut woken: HashSet<ChunkCoord> = HashSet::new();
                        {
                            let mut det = detector.lock().expect("detector mutex poisoned");
                            for coord in coords {
                                if subscribed.insert(coord) {
                                    if world.wake_if_paused(coord) {
                                        woken.insert(coord);
                                    }
                                    det.forget(coord);
                                }
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
                                    edits: vec![],
                                    removed: vec![],
                                    live_count: world.len(),
                                    compute_duration: Duration::ZERO,
                                    initial: false,
                                    is_tick: false,
                                })
                                .await
                                .expect("sim event channel closed during subscribe wake (hub gone)");
                        }
                    }
                    SimCmd::Unsubscribe(coords) => {
                        for coord in coords {
                            subscribed.remove(&coord);
                        }
                    }
                    SimCmd::SetOwned(owned) => {
                        world.set_owned_chunks(owned);
                    }
                }
            }
            _ = ticker.tick() => {
                let mut promotions = 0u64;
                // Drain pending promote requests into a local buffer so we can
                // acquire the detector lock once for all group-formation attempts.
                let mut promote_buf: Vec<PromoteRequest> = Vec::new();
                while promote_buf.len() < cfg.oscillator_promote_max_per_tick {
                    match promote_rx.try_recv() {
                        Ok(req) => promote_buf.push(req),
                        Err(_) => break,
                    }
                }
                if !promote_buf.is_empty() {
                    // Candidates that failed individual promotion and may form a group.
                    let mut group_candidates: Vec<PromoteRequest> = Vec::new();
                    for req in &promote_buf {
                        if subscribed.contains(&req.coord) { continue; }
                        if world.promote_oscillator(req.coord, req.period) {
                            promotions += 1;
                        } else {
                            group_candidates.push(*req);
                        }
                    }
                    if !group_candidates.is_empty() {
                        let groups: Vec<(Vec<ChunkCoord>, u8)> = group_candidates
                            .iter()
                            .filter_map(|req| {
                                collect_group(req.coord, req.period, &world, &subscribed, cfg.oscillator_max_group_size)
                                    .map(|members| (members, req.period))
                            })
                            .collect();
                        // Track which coords we already promoted as part of a group this
                        // tick to avoid double-promotion if two requests hit the same group.
                        let mut promoted_coords: HashSet<ChunkCoord> = HashSet::new();
                        for (members, period) in groups {
                            if members.iter().any(|c| promoted_coords.contains(c)) { continue; }
                            if world.promote_oscillator_group(&members, period) {
                                promotions += members.len() as u64;
                                // Forget all member rings so stale entries don't accumulate.
                                let mut det = detector.lock().expect("detector mutex poisoned");
                                for &c in &members { det.forget(c); }
                                drop(det);
                                promoted_coords.extend(members);
                            }
                        }
                    }
                }
                if promotions > 0 {
                    metrics.oscillator_promotions_total.inc_by(promotions);
                }
                let start = Instant::now();
                world.tick_into(&mut outcome);
                if !outcome.hash_reports.is_empty() || !outcome.removed.is_empty() {
                    let tracked;
                    {
                        let mut det = detector.lock().expect("detector mutex poisoned");
                        for &coord in &outcome.removed {
                            det.forget(coord);
                        }
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
                let hint = outcome.changed.len();
                let changed = collect(&world, outcome.changed.iter().copied(), hint);
                let removed: Vec<ChunkCoord> = outcome.removed.drain(..).collect();
                let now = world.tick_number();

                event_tx
                    .send(SimEvent {
                        tick: now,
                        changed,
                        edits: vec![],
                        removed,
                        live_count: world.len(),
                        compute_duration: elapsed,
                        initial: false,
                        is_tick: true,
                    })
                    .await
                    .expect("sim event channel closed during tick (hub gone)");

                if now >= next_snapshot_tick {
                    next_snapshot_tick = now + snapshot_interval_ticks;
                    let snaps = crate::snapshot::collect(&world);
                    io_tx
                        .send(IoCmd::Snapshot { tick: now, snaps })
                        .await
                        .expect("io channel closed during snapshot dispatch");
                }
            }
        }
    }
}

/// Flood-fill the set of live chunks that interact with `seed` via mutual edge
/// bits. Returns `None` if no group (>1 member) is found or if a subscribed coord
/// would be included.
///
/// Period correctness and external-isolation are validated later inside
/// `World::promote_oscillator_group`, which is the authoritative gate.
fn collect_group(
    seed: ChunkCoord,
    period: u8,
    world: &World,
    subscribed: &HashSet<ChunkCoord>,
    max_group: usize,
) -> Option<Vec<ChunkCoord>> {
    let _ = period; // period unused here; kept for call-site symmetry
    let mut group: Vec<ChunkCoord> = Vec::new();
    let mut visited: HashSet<ChunkCoord> = HashSet::new();
    let mut stack: Vec<ChunkCoord> = vec![seed];
    while let Some(coord) = stack.pop() {
        if !visited.insert(coord) { continue; }
        if subscribed.contains(&coord) { return None; }
        if group.len() >= max_group { return None; }
        let chunk = world.get_live_chunk(coord)?;
        group.push(coord);
        let (cx, cy) = coord;
        let e = chunk.edges();
        let candidates: [(u64, ChunkCoord); 8] = [
            (e.top,                    (cx, cy - 1)),
            (e.bottom,                 (cx, cy + 1)),
            (e.left,                   (cx - 1, cy)),
            (e.right,                  (cx + 1, cy)),
            (u64::from(e.corners[0]),  (cx - 1, cy - 1)),
            (u64::from(e.corners[1]),  (cx + 1, cy - 1)),
            (u64::from(e.corners[2]),  (cx - 1, cy + 1)),
            (u64::from(e.corners[3]),  (cx + 1, cy + 1)),
        ];
        for (bits, nb) in candidates {
            if bits != 0 && !visited.contains(&nb) { stack.push(nb); }
        }
        // Also expand reverse: neighbors that point toward this coord.
        for &nb in &[
            (cx, cy - 1), (cx, cy + 1), (cx - 1, cy), (cx + 1, cy),
            (cx - 1, cy - 1), (cx + 1, cy - 1), (cx - 1, cy + 1), (cx + 1, cy + 1),
        ] {
            if visited.contains(&nb) { continue; }
            if let Some(nb_chunk) = world.get_live_chunk(nb) {
                let (dx, dy) = (coord.0 - nb.0, coord.1 - nb.1);
                if nb_chunk.has_edge_toward(dx, dy) { stack.push(nb); }
            }
        }
    }
    if group.len() <= 1 { return None; }
    Some(group)
}

fn collect(world: &World, coords: impl Iterator<Item = ChunkCoord>, hint: usize) -> Vec<ChunkSnap> {
    let mut out = Vec::with_capacity(hint);
    for coord in coords {
        if let Some(chunk) = world.get_chunk(coord) {
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
