//! Atomic snapshot of the world. Format:
//!
//!   magic[8]="LAZOSWLD" version:u32=1 chunk_size:u8 tick:u64 count:u32
//!   { cx:i32 cy:i32 frozen_flag:u8 bits:[256]
//!     if frozen_flag != 0 { mask:[256] value:[256] } } * count
//!
//! All little-endian. No compression. Version/chunk_size mismatch → panic.

use protocol::{bits_to_rows, rows_to_bits, BITS_BYTES};
use simulation::{Chunk, FrozenMask, World, CHUNK_SIZE};
use std::io::Read;
#[cfg(test)]
use std::io::Write;
use std::path::Path;

const MAGIC: [u8; 8] = *b"LAZOSWLD";
const VERSION: u32 = 1;

/// Serialize the entire world to an in-memory buffer. Cheap (single pass over
/// chunks, no I/O) and called from the sim task so the IO task only does the
/// fsync + rename.
pub fn serialize(world: &World) -> Vec<u8> {
    let mut buf = Vec::with_capacity(world.len() * (8 + 1 + BITS_BYTES) + 32);
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.push(CHUNK_SIZE as u8);
    buf.extend_from_slice(&world.tick_number().to_le_bytes());
    let count = u32::try_from(world.len()).expect("chunk count > u32::MAX");
    buf.extend_from_slice(&count.to_le_bytes());
    // Sort for deterministic byte order (HashMap iter is non-deterministic).
    let mut entries: Vec<_> = world.iter_chunks().collect();
    entries.sort_by_key(|((cx, cy), _)| (*cx, *cy));
    for ((cx, cy), chunk) in entries {
        buf.extend_from_slice(&cx.to_le_bytes());
        buf.extend_from_slice(&cy.to_le_bytes());
        let bits = rows_to_bits(chunk.rows());
        match &chunk.frozen {
            None => {
                buf.push(0);
                buf.extend_from_slice(&bits);
            }
            Some(mask) => {
                buf.push(1);
                buf.extend_from_slice(&bits);
                buf.extend_from_slice(&rows_to_bits(&mask.mask));
                buf.extend_from_slice(&rows_to_bits(&mask.value));
            }
        }
    }
    buf
}

/// Atomic write: tmp → fsync → rename. Kept for the sync test path; runtime
/// snapshots flow through `crate::io_task` which does the same write off-task.
#[cfg(test)]
pub fn save(world: &World, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serialize(world);
    let tmp = path.with_extension("snap.tmp");
    {
        let file = std::fs::File::create(&tmp)?;
        let mut w = std::io::BufWriter::new(file);
        w.write_all(&bytes)?;
        w.flush()?;
        w.into_inner()?.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load(path: &Path) -> std::io::Result<World> {
    let file = std::fs::File::open(path)?;
    let mut r = std::io::BufReader::new(file);

    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    assert_eq!(magic, MAGIC, "snapshot magic mismatch");

    let mut buf4 = [0u8; 4];
    r.read_exact(&mut buf4)?;
    let version = u32::from_le_bytes(buf4);
    assert_eq!(version, VERSION, "snapshot version mismatch (got {version}, expected {VERSION})");

    let mut buf1 = [0u8; 1];
    r.read_exact(&mut buf1)?;
    assert_eq!(buf1[0] as usize, CHUNK_SIZE, "snapshot chunk_size mismatch");

    let mut buf8 = [0u8; 8];
    r.read_exact(&mut buf8)?;
    let tick = u64::from_le_bytes(buf8);

    r.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4);

    let mut world = World::new();
    world.set_tick_number(tick);
    let mut bits = [0u8; BITS_BYTES];
    for _ in 0..count {
        r.read_exact(&mut buf4)?;
        let cx = i32::from_le_bytes(buf4);
        r.read_exact(&mut buf4)?;
        let cy = i32::from_le_bytes(buf4);
        r.read_exact(&mut buf1)?;
        let frozen_flag = buf1[0];
        r.read_exact(&mut bits)?;
        let rows = bits_to_rows(&bits);
        let frozen = if frozen_flag == 0 {
            None
        } else {
            let mut m = [0u8; BITS_BYTES];
            let mut v = [0u8; BITS_BYTES];
            r.read_exact(&mut m)?;
            r.read_exact(&mut v)?;
            Some(std::sync::Arc::new(FrozenMask {
                mask: bits_to_rows(&m),
                value: bits_to_rows(&v),
            }))
        };
        world.insert_chunk((cx, cy), Chunk::from_rows_and_frozen(rows, frozen));
    }
    Ok(world)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let path = std::env::temp_dir().join(format!("lazos-snap-test-{}.snap", std::process::id()));
        let mut w = World::new();
        for i in 0..50 {
            w.set_cell(i, i * 2, true);
        }
        w.freeze_cell(-100, -100, true);
        for _ in 0..7 {
            w.tick();
        }
        save(&w, &path).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.tick_number(), w.tick_number());
        assert_eq!(loaded.len(), w.len());
        for ((c, ch), (c2, ch2)) in w.iter_chunks().zip(loaded.iter_chunks()) {
            assert_eq!(c, c2);
            assert_eq!(ch.rows(), ch2.rows());
            assert_eq!(ch.frozen.is_some(), ch2.frozen.is_some());
        }
        let _ = std::fs::remove_file(&path);
    }
}
