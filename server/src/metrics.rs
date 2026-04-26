//! Four Prometheus gauges. Updated from the hub task at end-of-tick.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Default)]
pub struct Metrics {
    live_chunks: AtomicU64,
    tick_rate_hz_milli: AtomicU64,    // store *1000 to keep the gauge as integer
    client_sessions: AtomicU64,
    tick_utilization_milli: AtomicU64, // store *1000 (so 500 = 0.5)
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn set_live_chunks(&self, v: u64) {
        self.live_chunks.store(v, Ordering::Relaxed);
    }

    pub fn set_tick_rate_hz(&self, v: f64) {
        self.tick_rate_hz_milli
            .store((v * 1000.0) as u64, Ordering::Relaxed);
    }

    pub fn set_client_sessions(&self, v: u64) {
        self.client_sessions.store(v, Ordering::Relaxed);
    }

    pub fn set_tick_utilization(&self, v: f64) {
        self.tick_utilization_milli
            .store((v * 1000.0) as u64, Ordering::Relaxed);
    }

    pub fn live_chunks(&self) -> u64 { self.live_chunks.load(Ordering::Relaxed) }
    pub fn tick_rate_hz_milli(&self) -> u32 {
        let v = self.tick_rate_hz_milli.load(Ordering::Relaxed);
        u32::try_from(v).expect("tick_rate_hz_milli exceeds u32 (rate sanity)")
    }
    pub fn tick_utilization_milli(&self) -> u32 {
        let v = self.tick_utilization_milli.load(Ordering::Relaxed);
        u32::try_from(v).expect("tick_utilization_milli exceeds u32")
    }

    pub fn render(&self) -> String {
        let live = self.live_chunks.load(Ordering::Relaxed);
        let hz = self.tick_rate_hz_milli.load(Ordering::Relaxed) as f64 / 1000.0;
        let peers = self.client_sessions.load(Ordering::Relaxed);
        let util = self.tick_utilization_milli.load(Ordering::Relaxed) as f64 / 1000.0;
        format!(
            "# TYPE lazos_live_chunks gauge\nlazos_live_chunks {live}\n\
             # TYPE lazos_tick_rate_hz gauge\nlazos_tick_rate_hz {hz}\n\
             # TYPE lazos_client_sessions gauge\nlazos_client_sessions {peers}\n\
             # TYPE lazos_tick_utilization gauge\nlazos_tick_utilization {util}\n"
        )
    }
}
