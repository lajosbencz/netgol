//! In-memory claim registry. Wraps [`ClaimStore`] and exposes the region/coord
//! derivations the hub needs without leaking file-I/O details into hub.rs.

use crate::claim_store::{Claim, ClaimStore};
use protocol::{Region, FLAG_FROZEN, FLAG_LOCKED, FLAG_OWNED};
use simulation::{ChunkCoord, CoordMap, CHUNK_SIZE, CHUNK_SIZE_I64};
use std::collections::HashSet;
use std::sync::Arc;

pub struct ClaimManager {
    store: Arc<ClaimStore>,
    pub claims: Vec<Claim>,
    pub claim_w: i32,
    pub claim_h: i32,
}

impl ClaimManager {
    pub async fn new(store: Arc<ClaimStore>, w: u32, h: u32) -> Self {
        let claims = store.all().await.unwrap_or_else(|e| {
            tracing::error!(err = %e, "load claims on startup");
            Vec::new()
        });
        Self { store, claims, claim_w: w as i32, claim_h: h as i32 }
    }

    /// Place or move a claim for `user_id`. Returns false if the target region
    /// overlaps any locked or frozen region (same-user replacement is allowed).
    pub fn try_create(
        &mut self,
        user_id: u32,
        cx: i32,
        cy: i32,
        regions: &[Region],
    ) -> bool {
        let cell_x = cx as i64 * CHUNK_SIZE_I64;
        let cell_y = cy as i64 * CHUNK_SIZE_I64;
        let w = self.claim_w as i64 * CHUNK_SIZE_I64;
        let h = self.claim_h as i64 * CHUNK_SIZE_I64;
        let overlaps = regions.iter().any(|r| {
            if r.flags & (FLAG_LOCKED | FLAG_FROZEN) == 0 { return false; }
            // Same user's own prior claim is replaced — no conflict.
            if r.flags & FLAG_OWNED != 0 && r.owner == user_id { return false; }
            // Geometric overlap check.
            !(cell_x + w <= r.x
                || r.x + i64::from(r.w) <= cell_x
                || cell_y + h <= r.y
                || r.y + i64::from(r.h) <= cell_y)
        });
        if overlaps { return false; }
        self.claims.retain(|c| c.user_id != user_id);
        self.claims.push(Claim { user_id, cx, cy });
        true
    }

    /// Remove the claim for `user_id`. Returns false if none existed.
    pub fn try_delete(&mut self, user_id: u32) -> bool {
        let before = self.claims.len();
        self.claims.retain(|c| c.user_id != user_id);
        self.claims.len() < before
    }

    /// Spawn a background task to persist a create.
    pub fn persist_create(&self, email_key: &str, user_id: u32, cx: i32, cy: i32) {
        let store = Arc::clone(&self.store);
        let key = email_key.to_string();
        tokio::spawn(async move {
            if let Err(e) = store.put(&key, cx, cy, user_id).await {
                tracing::error!(err = %e, "persist claim create");
            }
        });
    }

    /// Spawn a background task to persist a delete.
    pub fn persist_delete(&self, email_key: &str) {
        let store = Arc::clone(&self.store);
        let key = email_key.to_string();
        tokio::spawn(async move {
            if let Err(e) = store.delete(&key).await {
                tracing::error!(err = %e, "persist claim delete");
            }
        });
    }

    /// Build the combined region list (static + claims) for broadcast.
    pub fn build_regions(&self, static_regions: &[Region]) -> Arc<[Region]> {
        let mut v: Vec<Region> = static_regions.to_vec();
        for c in &self.claims {
            v.push(Region {
                x: c.cx as i64 * CHUNK_SIZE_I64,
                y: c.cy as i64 * CHUNK_SIZE_I64,
                w: (self.claim_w as u32) * CHUNK_SIZE as u32,
                h: (self.claim_h as u32) * CHUNK_SIZE as u32,
                flags: FLAG_LOCKED | FLAG_OWNED,
                owner: c.user_id,
            });
        }
        Arc::from(v)
    }

    /// `HashSet` of all chunk coords covered by any claim (hub fan-out guard).
    pub fn owned_chunk_set(&self) -> HashSet<ChunkCoord> {
        let mut set = HashSet::new();
        for c in &self.claims {
            for dy in 0..self.claim_h {
                for dx in 0..self.claim_w {
                    set.insert((c.cx + dx, c.cy + dy));
                }
            }
        }
        set
    }

    /// `CoordMap<()>` for the simulation's owned-chunk halo isolation.
    pub fn owned_coord_map(&self) -> CoordMap<()> {
        let mut map = CoordMap::default();
        for c in &self.claims {
            for dy in 0..self.claim_h {
                for dx in 0..self.claim_w {
                    map.insert((c.cx + dx, c.cy + dy), ());
                }
            }
        }
        map
    }

    /// Top-left chunk coord of `user_id`'s claim, if any.
    pub fn find_for_user(&self, user_id: u32) -> Option<ChunkCoord> {
        self.claims.iter().find(|c| c.user_id == user_id).map(|c| (c.cx, c.cy))
    }
}
