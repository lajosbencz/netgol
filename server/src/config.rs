use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub bind: String,
    pub metrics_bind: String,
    pub tick_hz: u32,
    pub max_live_chunks: usize,
    pub peer_outbound_capacity: usize,
    pub snapshot_path: PathBuf,
    pub snapshot_interval_ticks: u64,
    pub regions_path: PathBuf,
    /// Hard cap on a single peer's subscribed chunk set. Bounds per-peer server
    /// memory (peer.subscribed + chunk_subs reverse index entries). Overridable
    /// via the `CLIENT_MAX_CHUNKS` env var; defaults to the wire u16 limit.
    #[serde(default = "default_client_max_chunks")]
    pub client_max_chunks: u32,

    #[serde(default = "default_osc_interval_ms")]
    pub oscillator_detection_interval_ms: u64,
    #[serde(default = "default_osc_budget")]
    pub oscillator_detection_max_chunks_per_step: usize,
    #[serde(default = "default_osc_promote_per_tick")]
    pub oscillator_promote_max_per_tick: usize,
}

fn default_client_max_chunks() -> u32 { 65535 }
fn default_osc_interval_ms() -> u64 { 250 }
fn default_osc_budget() -> usize { 1000 }
fn default_osc_promote_per_tick() -> usize { 256 }

impl Config {
    pub fn load(path: &Path) -> Self {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read config {}: {e}", path.display()));
        let mut cfg: Self = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("parse config {}: {e}", path.display()));
        if let Ok(v) = std::env::var("CLIENT_MAX_CHUNKS") {
            cfg.client_max_chunks = v.parse()
                .unwrap_or_else(|e| panic!("CLIENT_MAX_CHUNKS={v:?}: {e}"));
        }
        cfg
    }
}
