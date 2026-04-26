//! Dedicated I/O task. Owns the snapshot file and the write-ahead log.
//!
//! Responsibilities:
//!   * Append per-tick edit batches to the WAL and `fsync` once per record so
//!     recent edits survive a crash without blocking the sim tick loop.
//!   * Write snapshots atomically (`tmp → fsync → rename`) and truncate the WAL
//!     on success so it stays bounded.
//!
//! The sim sends [`IoCmd`]s over a bounded channel and never blocks waiting for
//! them. Channel-full = invariant violation (panic) so I/O backpressure surfaces
//! immediately rather than as silent data loss.

use crate::metrics::Metrics;
use protocol::EditCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

pub const WAL_MAGIC: [u8; 8] = *b"LAZOSWAL";
pub const WAL_VERSION: u32 = 1;

/// Bounded so a stalled IO task fails loud - sim panics on full channel rather
/// than accumulating WAL records in memory.
const IO_CMD_CAPACITY: usize = 256;

#[derive(Debug)]
pub enum IoCmd {
    /// Edits applied this sim tick. Must arrive in tick order.
    AppendEdits { tick: u64, cells: Vec<EditCell> },
    /// Pre-serialized snapshot bytes, for the IO task to write atomically.
    Snapshot { tick: u64, bytes: Vec<u8> },
}

pub struct IoHandles {
    pub tx: mpsc::Sender<IoCmd>,
}

pub fn spawn(snapshot_path: PathBuf, chunk_size: u8, metrics: Arc<Metrics>) -> IoHandles {
    let (tx, rx) = mpsc::channel(IO_CMD_CAPACITY);
    tokio::spawn(run(snapshot_path, chunk_size, rx, metrics));
    IoHandles { tx }
}

pub fn wal_path(snapshot_path: &Path) -> PathBuf {
    snapshot_path.with_extension("wal")
}

async fn run(
    snapshot_path: PathBuf,
    chunk_size: u8,
    mut rx: mpsc::Receiver<IoCmd>,
    metrics: Arc<Metrics>,
) {
    let wal_path = wal_path(&snapshot_path);
    let mut wal = open_wal_for_append(&wal_path, chunk_size)
        .await
        .unwrap_or_else(|e| panic!("open wal {}: {e}", wal_path.display()));

    while let Some(cmd) = rx.recv().await {
        match cmd {
            IoCmd::AppendEdits { tick, cells } => {
                let t = Instant::now();
                append_edits(&mut wal, tick, &cells)
                    .await
                    .unwrap_or_else(|e| panic!("wal append: {e}"));
                metrics.wal_fsync_seconds.observe(t.elapsed().as_secs_f64());
            }
            IoCmd::Snapshot { tick, bytes } => {
                let len = bytes.len();
                let t = Instant::now();
                write_snapshot_atomic(&snapshot_path, &bytes)
                    .await
                    .unwrap_or_else(|e| panic!("snapshot write: {e}"));
                wal = truncate_wal(&wal_path, chunk_size)
                    .await
                    .unwrap_or_else(|e| panic!("wal truncate: {e}"));
                metrics.snapshot_seconds.observe(t.elapsed().as_secs_f64());
                metrics.snapshot_bytes.set(len as i64);
                tracing::info!(tick, "snapshot saved; wal truncated");
            }
        }
    }
}

async fn open_wal_for_append(path: &Path, chunk_size: u8) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)
        .await?;
    let len = f.metadata().await?.len();
    if len == 0 {
        write_wal_header(&mut f, chunk_size).await?;
        f.sync_all().await?;
    } else {
        verify_wal_header(path, chunk_size).await?;
    }
    Ok(f)
}

async fn write_wal_header(f: &mut File, chunk_size: u8) -> std::io::Result<()> {
    f.write_all(&WAL_MAGIC).await?;
    f.write_all(&WAL_VERSION.to_le_bytes()).await?;
    f.write_all(&[chunk_size]).await?;
    Ok(())
}

async fn verify_wal_header(path: &Path, chunk_size: u8) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut f = File::open(path).await?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).await?;
    assert_eq!(magic, WAL_MAGIC, "wal magic mismatch at {}", path.display());
    let mut buf4 = [0u8; 4];
    f.read_exact(&mut buf4).await?;
    let version = u32::from_le_bytes(buf4);
    assert_eq!(version, WAL_VERSION, "wal version mismatch (got {version}, expected {WAL_VERSION})");
    let mut buf1 = [0u8; 1];
    f.read_exact(&mut buf1).await?;
    assert_eq!(buf1[0], chunk_size, "wal chunk_size mismatch");
    Ok(())
}

async fn append_edits(f: &mut File, tick: u64, cells: &[EditCell]) -> std::io::Result<()> {
    let n = u32::try_from(cells.len()).expect("edit batch > u32::MAX cells");
    // One contiguous write keeps the per-record cost a single syscall + fsync.
    let mut buf: Vec<u8> = Vec::with_capacity(8 + 4 + cells.len() * 11);
    buf.extend_from_slice(&tick.to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    for c in cells {
        buf.extend_from_slice(&c.cx.to_le_bytes());
        buf.extend_from_slice(&c.cy.to_le_bytes());
        buf.push(c.lx);
        buf.push(c.ly);
        buf.push(u8::from(c.alive));
    }
    f.write_all(&buf).await?;
    f.sync_all().await?;
    Ok(())
}

async fn write_snapshot_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("snap.tmp");
    {
        let mut f = File::create(&tmp).await?;
        f.write_all(bytes).await?;
        f.sync_all().await?;
    }
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

async fn truncate_wal(path: &Path, chunk_size: u8) -> std::io::Result<File> {
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .read(true)
        .open(path)
        .await?;
    write_wal_header(&mut f, chunk_size).await?;
    f.sync_all().await?;
    // Reopen in append mode so subsequent writes go to EOF correctly.
    drop(f);
    let f = OpenOptions::new().create(false).append(true).read(true).open(path).await?;
    f.sync_all().await?;
    Ok(f)
}

/// Replay the WAL onto `world` (read at boot, before sim spawns). For each
/// record with `tick > world.tick_number()` we step the world forward to that
/// tick, then apply the edits - same convention sim uses live: edits arrive
/// after the previous tick and before the next.
pub fn replay_into(world: &mut simulation::World, snapshot_path: &Path, chunk_size: u8) -> std::io::Result<u64> {
    use std::fs::File as StdFile;
    use std::io::{BufReader, Read};

    let path = wal_path(snapshot_path);
    let mut f = match StdFile::open(&path) {
        Ok(f) => BufReader::new(f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };

    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)?;
    assert_eq!(magic, WAL_MAGIC, "wal magic mismatch at {}", path.display());
    let mut buf4 = [0u8; 4];
    f.read_exact(&mut buf4)?;
    let version = u32::from_le_bytes(buf4);
    assert_eq!(version, WAL_VERSION, "wal version mismatch");
    let mut buf1 = [0u8; 1];
    f.read_exact(&mut buf1)?;
    assert_eq!(buf1[0], chunk_size, "wal chunk_size mismatch");

    let mut applied = 0u64;
    let mut buf8 = [0u8; 8];
    loop {
        match f.read_exact(&mut buf8) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        let tick = u64::from_le_bytes(buf8);
        f.read_exact(&mut buf4)?;
        let n = u32::from_le_bytes(buf4) as usize;

        if tick < world.tick_number() {
            // Already covered by snapshot; skip the cells.
            let mut sink = vec![0u8; n * 11];
            f.read_exact(&mut sink)?;
            continue;
        }

        // Live sim convention: WAL `tick` = `world.tick_number()` at the moment
        // edits were applied (after that tick's step, before the next). So step
        // forward to `tick` first, then set cells.
        while world.tick_number() < tick {
            world.tick();
        }

        for _ in 0..n {
            f.read_exact(&mut buf4)?;
            let cx = i32::from_le_bytes(buf4);
            f.read_exact(&mut buf4)?;
            let cy = i32::from_le_bytes(buf4);
            let mut b3 = [0u8; 3];
            f.read_exact(&mut b3)?;
            let lx = b3[0] as i64;
            let ly = b3[1] as i64;
            let alive = b3[2] != 0;
            let ax = i64::from(cx) * simulation::CHUNK_SIZE_I64 + lx;
            let ay = i64::from(cy) * simulation::CHUNK_SIZE_I64 + ly;
            world.set_cell(ax, ay, alive);
        }
        applied += 1;
    }
    Ok(applied)
}
