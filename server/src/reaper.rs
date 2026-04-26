//! Chunk eviction policy.
//!
//! Hard-immortal:
//!   1. Frozen chunks (`is_frozen`).
//!   2. Chunks currently subscribed by any peer.
//!
//! Soft sort over the rest, reap-first ascending by:
//!   ( -ticks_since_last_in_viewport,  // older = smaller (more reapable)
//!     live_cell_count,                  // emptier = smaller (more reapable)
//!     -manhattan_distance_from_origin )// farther = smaller (more reapable)

use simulation::ChunkCoord;
use std::collections::{HashMap, HashSet};

pub struct ReapInfo {
    pub live_count: u32,
    pub is_frozen: bool,
}

/// Returns the chunks to remove to bring `count` down to `cap`.
pub fn pick_reapable(
    chunks: &HashMap<ChunkCoord, ReapInfo>,
    subscribed: &HashSet<ChunkCoord>,
    last_seen_tick: &HashMap<ChunkCoord, u64>,
    now: u64,
    cap: usize,
) -> Vec<ChunkCoord> {
    let live = chunks.len();
    if live <= cap {
        return Vec::new();
    }
    let mut candidates: Vec<(ChunkCoord, i64, u32, i64)> = Vec::with_capacity(live);
    for (&coord, info) in chunks {
        if info.is_frozen || subscribed.contains(&coord) {
            continue;
        }
        let last = last_seen_tick.get(&coord).copied().unwrap_or(0);
        let ticks_since = now.saturating_sub(last) as i64;
        let manhattan = (coord.0.unsigned_abs() as i64) + (coord.1.unsigned_abs() as i64);
        candidates.push((coord, ticks_since, info.live_count, manhattan));
    }
    candidates.sort_by(|a, b| (-a.1, a.2, -a.3).cmp(&(-b.1, b.2, -b.3)));
    let to_reap = live - cap;
    candidates.into_iter().take(to_reap).map(|(c, _, _, _)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(live: u32, frozen: bool) -> ReapInfo {
        ReapInfo { live_count: live, is_frozen: frozen }
    }

    #[test]
    fn frozen_and_subscribed_are_immortal() {
        let mut chunks = HashMap::new();
        chunks.insert((0, 0), info(5, true));
        chunks.insert((1, 0), info(5, false));
        chunks.insert((2, 0), info(5, false));
        let mut subs = HashSet::new();
        subs.insert((1, 0));
        let reap = pick_reapable(&chunks, &subs, &HashMap::new(), 0, 2);
        assert_eq!(reap, vec![(2, 0)]);
    }

    #[test]
    fn under_cap_returns_empty() {
        let mut chunks = HashMap::new();
        chunks.insert((0, 0), info(1, false));
        assert!(pick_reapable(&chunks, &HashSet::new(), &HashMap::new(), 0, 100).is_empty());
    }

    #[test]
    fn reaps_oldest_first() {
        let mut chunks = HashMap::new();
        chunks.insert((0, 0), info(5, false));
        chunks.insert((1, 0), info(5, false));
        chunks.insert((10, 10), info(5, false));
        let mut last_seen = HashMap::new();
        last_seen.insert((0, 0), 100);
        last_seen.insert((1, 0), 90);
        last_seen.insert((10, 10), 0);
        let reap = pick_reapable(&chunks, &HashSet::new(), &last_seen, 100, 1);
        assert_eq!(reap.len(), 2);
        assert!(reap.contains(&(10, 10)));
        assert!(reap.contains(&(1, 0)));
    }
}
