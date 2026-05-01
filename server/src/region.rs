//! Region registry: parses `regions.toml`, exposes the wire-shape `Region` list,
//! and provides per-chunk frozen-mask + locked-cell lookup for sim and hub.
//!
//! Regions are rectangular and carry independent flags (`FROZEN`, `LOCKED`,
//! `OWNED`). The optional `pattern` text initialises the live cells inside the
//! region using `#` (alive), `.` (dead/skip), and ` ` (skip).

use protocol::{Region, FLAG_FROZEN, FLAG_LOCKED, FLAG_OWNED};
use serde::Deserialize;
use simulation::World;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct File {
    #[serde(default)]
    regions: Vec<RegionEntry>,
}

#[derive(Debug, Deserialize)]
struct RegionEntry {
    origin: [i64; 2],
    #[serde(default)]
    flags: Option<Vec<String>>,
    #[serde(default)]
    owner: Option<u32>,
    /// Optional pattern that seeds live cells inside the region.
    /// `#` = alive, `.` = dead (no-op since cells start dead), ` ` = skip.
    /// The pattern's bounding box also defines the region's `w` x `h`.
    #[serde(default)]
    pattern: Option<String>,
    /// If `pattern` is omitted, `w` and `h` define the region's extent directly.
    #[serde(default)]
    w: Option<u32>,
    #[serde(default)]
    h: Option<u32>,
}

/// Loads `regions.toml`, applies any embedded `pattern` cells to the world,
/// and returns the wire-form region list.
pub fn load(world: &mut World, path: &Path) -> Vec<Region> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("no regions file at {}; skipping", path.display());
            return Vec::new();
        }
        Err(e) => panic!("read regions {}: {e}", path.display()),
    };
    let f: File = toml::from_str(&text)
        .unwrap_or_else(|e| panic!("parse regions {}: {e}", path.display()));

    let mut out = Vec::with_capacity(f.regions.len());
    let mut total_cells: u64 = 0;
    for entry in f.regions {
        let flags = parse_flags(entry.flags.as_deref().unwrap_or_default());
        let (ox, oy) = (entry.origin[0], entry.origin[1]);

        let (w, h) = if let Some(p) = &entry.pattern {
            let (pw, ph) = pattern_extent(p);
            (pw, ph)
        } else {
            (entry.w.unwrap_or(0), entry.h.unwrap_or(0))
        };

        if let Some(pat) = &entry.pattern {
            // Clear the full rect first so snapshot state doesn't bleed through dots.
            for dy in 0..h as i64 {
                for dx in 0..w as i64 {
                    world.set_cell(ox + dx, oy + dy, false);
                }
            }
            for (row_idx, line) in pat.lines().enumerate() {
                for (col_idx, ch) in line.chars().enumerate() {
                    let alive = match ch {
                        '#' => true,
                        '.' | ' ' => continue,
                        _ => panic!("region pattern char {ch:?} invalid (use '#', '.', ' ')"),
                    };
                    let x = ox + col_idx as i64;
                    let y = oy + row_idx as i64;
                    world.set_cell(x, y, alive);
                    total_cells += 1;
                }
            }
        }

        let region = Region { x: ox, y: oy, w, h, flags, owner: entry.owner.unwrap_or(0) };

        // Apply FROZEN immediately to the simulation world so the bit-parallel
        // step's per-chunk mask is correctly populated. (This is the only flag
        // sim cares about; LOCKED and OWNED are hub-side concerns.)
        if region.flags & FLAG_FROZEN != 0 {
            apply_frozen(world, &region);
        }

        out.push(region);
    }

    for r in &out {
        tracing::info!(
            x = r.x, y = r.y, w = r.w, h = r.h, flags = r.flags,
            "region"
        );
    }
    tracing::info!(regions = out.len(), live_cells = total_cells, "loaded regions");
    out
}

/// Mark every cell inside `region` as frozen at its current alive value.
fn apply_frozen(world: &mut World, r: &Region) {
    world.freeze_rect(r.x, r.y, r.w, r.h);
}

/// Returns whether `editor_uid` is allowed to edit the cell at `(x, y)`.
/// - No locking region: always allowed.
/// - Static locked region (LOCKED without OWNED): never allowed.
/// - Owned region (LOCKED | OWNED): allowed only for the owner.
pub fn can_edit(regions: &[Region], x: i64, y: i64, editor_uid: Option<u32>) -> bool {
    for r in regions {
        if !contains(r, x, y) { continue; }
        if r.flags & FLAG_LOCKED == 0 { continue; }
        if r.flags & FLAG_OWNED != 0 {
            return editor_uid == Some(r.owner);
        }
        return false;
    }
    true
}


fn contains(r: &Region, x: i64, y: i64) -> bool {
    x >= r.x && x < r.x + i64::from(r.w) && y >= r.y && y < r.y + i64::from(r.h)
}

fn pattern_extent(p: &str) -> (u32, u32) {
    let h = p.lines().count() as u32;
    let w = p.lines().map(str::len).max().unwrap_or(0) as u32;
    (w, h)
}

fn parse_flags(names: &[String]) -> u8 {
    let mut f = 0u8;
    for name in names {
        match name.as_str() {
            "frozen" => f |= FLAG_FROZEN,
            "locked" => f |= FLAG_LOCKED,
            "owned" => f |= FLAG_OWNED,
            other => panic!("unknown region flag {other:?} (use frozen|locked|owned)"),
        }
    }
    f
}
