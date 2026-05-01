//! Region registry: parses `regions.toml`, exposes the wire-shape `Region` list,
//! and provides per-chunk frozen-mask + locked-cell lookup for sim and hub.
//!
//! Regions are rectangular and carry independent flags (`FROZEN`, `LOCKED`,
//! `OWNED`). The optional `pattern` text initialises the live cells inside the
//! region using `#` (alive), `.` (dead/skip), and ` ` (skip).

use protocol::{Region, FLAG_FROZEN, FLAG_LOCKED, FLAG_OWNED};
use simulation::{CoordMap, CHUNK_SIZE_I64};
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

    // Regions are the sole source of truth for frozen state. Strip any stale
    // masks carried over from the snapshot (e.g. after a region origin change).
    world.clear_all_frozen();

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
            // clear_all_frozen already stripped stale snapshot masks, so set_cell
            // works unconditionally here. freeze_rect below handles FLAG_FROZEN.
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
                    world.set_cell(ox + col_idx as i64, oy + row_idx as i64, alive);
                    total_cells += 1;
                }
            }
        }

        let region = Region { x: ox, y: oy, w, h, flags, owner: entry.owner.unwrap_or(0) };

        if region.flags & FLAG_FROZEN != 0 {
            world.freeze_rect(region.x, region.y, region.w, region.h);
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


/// All chunks touched by regions carrying FLAG_LOCKED, for one-way halo isolation.
pub fn locked_chunks(regions: &[Region]) -> CoordMap<()> {
    let mut map = CoordMap::default();
    for r in regions {
        if r.flags & FLAG_LOCKED == 0 { continue; }
        let cx0 = r.x.div_euclid(CHUNK_SIZE_I64) as i32;
        let cy0 = r.y.div_euclid(CHUNK_SIZE_I64) as i32;
        let cx1 = (r.x + i64::from(r.w) - 1).div_euclid(CHUNK_SIZE_I64) as i32;
        let cy1 = (r.y + i64::from(r.h) - 1).div_euclid(CHUNK_SIZE_I64) as i32;
        for cy in cy0..=cy1 {
            for cx in cx0..=cx1 {
                map.insert((cx, cy), ());
            }
        }
    }
    map
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

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{FLAG_FROZEN, FLAG_LOCKED, FLAG_OWNED};
    use simulation::World;

    fn simple_region(x: i64, y: i64, w: u32, h: u32, flags: u8) -> Region {
        Region { x, y, w, h, flags, owner: 0 }
    }

    #[test]
    fn locked_chunks_covers_partial_chunk() {
        // Region that straddles two chunks horizontally.
        let r = simple_region(-10, -10, 20, 5, FLAG_LOCKED);
        let chunks = locked_chunks(&[r]);
        // x: -10 to 9 → chunk -1 (contains -10) and chunk 0 (contains 0..9)
        assert!(chunks.contains_key(&(-1, -1)));
        assert!(chunks.contains_key(&(0, -1)));
        // chunk 1 not touched
        assert!(!chunks.contains_key(&(1, -1)));
    }

    #[test]
    fn locked_chunks_skips_non_locked() {
        let regions = vec![
            simple_region(0, 0, 64, 64, FLAG_FROZEN),           // frozen only
            simple_region(64, 0, 64, 64, FLAG_LOCKED | FLAG_OWNED), // owned
            simple_region(128, 0, 64, 64, FLAG_LOCKED),          // locked
        ];
        let chunks = locked_chunks(&regions);
        assert!(!chunks.contains_key(&(0, 0)));  // frozen-only: not included
        assert!(chunks.contains_key(&(1, 0)));   // owned+locked: included
        assert!(chunks.contains_key(&(2, 0)));   // locked: included
    }

    #[test]
    fn pattern_load_clears_stale_cells() {
        let toml = r#"
[[regions]]
name = "test"
origin = [0, 0]
flags = ["frozen", "locked"]
pattern = """
#..
..#
"""
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml).unwrap();

        let mut world = World::new();
        // Seed a stale alive cell at (1,0) — should be cleared by the pattern's dot.
        world.set_cell(1, 0, true);
        assert!(world.get_chunk((0, 0)).map_or(false, |c| c.rows()[0] & 2 != 0));

        load(&mut world, tmp.path());

        let chunk = world.get_chunk((0, 0)).expect("chunk exists");
        let row0 = chunk.rows()[0];
        assert!(row0 & 1 != 0,  "cell (0,0) should be alive (#)");
        assert!(row0 & 2 == 0,  "cell (1,0) should be dead (. cleared)");
        assert!(row0 & 4 == 0,  "cell (2,0) should be dead (.)");
        let row1 = chunk.rows()[1];
        assert!(row1 & 4 != 0,  "cell (2,1) should be alive (#)");
    }

    #[test]
    fn locked_only_pattern_is_not_frozen() {
        let toml = r#"
[[regions]]
name = "gun"
origin = [0, 0]
flags = ["locked"]
pattern = """
##
##
"""
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml).unwrap();

        let mut world = World::new();
        load(&mut world, tmp.path());

        let chunk = world.get_chunk((0, 0)).expect("chunk placed");
        assert!(!chunk.is_frozen(), "locked-only pattern must not be frozen");
    }

    #[test]
    fn frozen_pattern_is_frozen() {
        let toml = r#"
[[regions]]
name = "stamp"
origin = [0, 0]
flags = ["frozen", "locked"]
pattern = """
#.
.#
"""
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml).unwrap();

        let mut world = World::new();
        load(&mut world, tmp.path());

        let chunk = world.get_chunk((0, 0)).expect("chunk placed");
        assert!(chunk.is_frozen(), "frozen+locked pattern must be frozen");
    }
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
