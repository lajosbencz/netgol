//! Prometheus metrics. Owned by the hub task; instrumented from sim/io tasks
//! through cheap `Arc<Metrics>` clones.
//!
//! Naming convention: `lazos_<subsystem>_<unit>` for everything new.

use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntGauge, Registry, TextEncoder,
};
use std::sync::Arc;

pub struct Metrics {
    registry: Registry,

    // World/sim gauges (also surfaced to clients via Stats heartbeat).
    pub live_chunks: IntGauge,
    pub client_sessions: IntGauge,
    pub tick_rate_hz_milli: IntGauge,    // value × 1000 to keep integer
    pub tick_utilization_milli: IntGauge, // value × 1000 (so 500 = 0.5)

    // Sim compute.
    pub tick_compute_seconds: Histogram,

    // Broadcast / fan-out.
    pub broadcast_bytes_per_tick: Histogram,
    pub broadcast_messages_per_tick: Histogram,
    pub bytes_sent_total: IntCounter,
    pub messages_sent_total: IntCounter,

    // Per-peer queue health (max across peers, sampled per tick).
    pub peer_outbound_depth_max: Histogram,

    // Drops / faults.
    pub peer_dropped_backpressure_total: IntCounter,

    // Edits.
    pub edits_total: IntCounter,

    // Message-type counters (cheap to keep separate).
    pub chunk_state_sent_total: IntCounter,
    pub chunk_delta_sent_total: IntCounter,
    pub reaped_chunks_total: IntCounter,

    // I/O task.
    pub wal_fsync_seconds: Histogram,
    pub snapshot_seconds: Histogram,
    pub snapshot_bytes: IntGauge,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();

        macro_rules! gauge {
            ($name:expr, $help:expr) => {{
                let g = IntGauge::new($name, $help).expect("gauge");
                registry.register(Box::new(g.clone())).expect("register gauge");
                g
            }};
        }
        macro_rules! counter {
            ($name:expr, $help:expr) => {{
                let c = IntCounter::new($name, $help).expect("counter");
                registry.register(Box::new(c.clone())).expect("register counter");
                c
            }};
        }
        macro_rules! histogram {
            ($name:expr, $help:expr, $buckets:expr) => {{
                let opts = HistogramOpts::new($name, $help).buckets($buckets.to_vec());
                let h = Histogram::with_opts(opts).expect("histogram");
                registry.register(Box::new(h.clone())).expect("register histogram");
                h
            }};
        }

        // Tick at 10 Hz means 100 ms budget. Buckets cover sub-ms to over-budget.
        const TICK_BUCKETS: [f64; 10] =
            [0.0005, 0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5];
        // SSD fsync usually <1 ms, but cloud volumes can spike to 100 ms+.
        const FSYNC_BUCKETS: [f64; 8] =
            [0.0005, 0.001, 0.002, 0.005, 0.01, 0.05, 0.1, 0.5];
        // Snapshot writes are bigger; budget tens of seconds in worst case.
        const SNAPSHOT_BUCKETS: [f64; 8] = [0.01, 0.05, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0];
        // Bytes per tick: 1 KiB .. 32 MiB.
        const BYTES_PER_TICK_BUCKETS: [f64; 8] = [
            1_024.0, 8_192.0, 32_768.0, 131_072.0, 524_288.0, 2_097_152.0, 8_388_608.0,
            33_554_432.0,
        ];
        // Messages per tick: 1 .. 32 k.
        const MSGS_PER_TICK_BUCKETS: [f64; 7] =
            [1.0, 8.0, 64.0, 512.0, 2_048.0, 8_192.0, 32_768.0];
        // Peer queue depth: 1 .. peer_outbound_capacity.
        const QUEUE_DEPTH_BUCKETS: [f64; 7] =
            [1.0, 8.0, 64.0, 256.0, 1_024.0, 4_096.0, 16_384.0];

        let live_chunks = gauge!("lazos_live_chunks", "Number of live (non-empty) chunks in the world");
        let client_sessions = gauge!("lazos_client_sessions", "Active websocket peers");
        let tick_rate_hz_milli = gauge!(
            "lazos_tick_rate_hz_milli",
            "Observed sim tick rate in Hz × 1000"
        );
        let tick_utilization_milli = gauge!(
            "lazos_tick_utilization_milli",
            "Fraction of tick budget spent computing × 1000 (1000 = 100%)"
        );

        let tick_compute_seconds = histogram!(
            "lazos_tick_compute_seconds",
            "Wall time spent in sim compute per tick",
            TICK_BUCKETS
        );

        let broadcast_bytes_per_tick = histogram!(
            "lazos_broadcast_bytes_per_tick",
            "Total bytes fanned out across all peers per tick",
            BYTES_PER_TICK_BUCKETS
        );
        let broadcast_messages_per_tick = histogram!(
            "lazos_broadcast_messages_per_tick",
            "Total messages fanned out across all peers per tick",
            MSGS_PER_TICK_BUCKETS
        );
        let bytes_sent_total = counter!(
            "lazos_bytes_sent_total",
            "Cumulative bytes successfully enqueued to peer outbound channels"
        );
        let messages_sent_total = counter!(
            "lazos_messages_sent_total",
            "Cumulative messages successfully enqueued to peer outbound channels"
        );

        let peer_outbound_depth_max = histogram!(
            "lazos_peer_outbound_depth_max",
            "Max peer outbound queue depth observed at tick boundary",
            QUEUE_DEPTH_BUCKETS
        );

        let peer_dropped_backpressure_total = counter!(
            "lazos_peer_dropped_backpressure_total",
            "Peers dropped because their outbound channel was full"
        );

        let edits_total = counter!("lazos_edits_total", "Cell edits accepted from clients");

        let chunk_state_sent_total = counter!(
            "lazos_chunk_state_sent_total",
            "ChunkState messages sent (subscribe path)"
        );
        let chunk_delta_sent_total = counter!(
            "lazos_chunk_delta_sent_total",
            "ChunkDelta messages sent (per-tick changes)"
        );
        let reaped_chunks_total = counter!(
            "lazos_reaped_chunks_total",
            "Chunks reaped under max_live_chunks pressure"
        );

        let wal_fsync_seconds = histogram!(
            "lazos_wal_fsync_seconds",
            "Per-tick WAL append + fsync duration",
            FSYNC_BUCKETS
        );
        let snapshot_seconds = histogram!(
            "lazos_snapshot_seconds",
            "Atomic snapshot write + fsync duration",
            SNAPSHOT_BUCKETS
        );
        let snapshot_bytes = gauge!("lazos_snapshot_bytes", "Size in bytes of the most recent snapshot");

        Arc::new(Self {
            registry,
            live_chunks,
            client_sessions,
            tick_rate_hz_milli,
            tick_utilization_milli,
            tick_compute_seconds,
            broadcast_bytes_per_tick,
            broadcast_messages_per_tick,
            bytes_sent_total,
            messages_sent_total,
            peer_outbound_depth_max,
            peer_dropped_backpressure_total,
            edits_total,
            chunk_state_sent_total,
            chunk_delta_sent_total,
            reaped_chunks_total,
            wal_fsync_seconds,
            snapshot_seconds,
            snapshot_bytes,
        })
    }

    pub fn render(&self) -> String {
        let mf = self.registry.gather();
        let mut buf = Vec::with_capacity(8 * 1024);
        TextEncoder::new()
            .encode(&mf, &mut buf)
            .expect("prometheus text encode");
        String::from_utf8(buf).expect("prometheus output is utf-8")
    }
}
