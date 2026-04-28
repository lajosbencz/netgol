# netgol

<img src="assets/glider.svg" alt="glider lifecycle" width="90" height="90" align="left">

A journey into optimizations through Conway's Game of Life.

One shared, persistent, effectively-infinite world that any number of clients can pan, zoom, and edit in real time.

<br clear="left">

## What's interesting

- **Three detached tokio tasks**:
  - `sim` owns the world
  - `hub` owns peers and a passive chunk mirror
  - `io` owns peristence to disk
- **Server-authoritative, sparse chunks.**
  The grid is a `HashMap<(i32,i32), Chunk>` of 64x64 bitsets - only live regions cost memory.
- **Bit-parallel kernel.**
  Each chunk steps via a half-adder cascade across its 8 shifted neighbors;
  one tick across a packed 64x64 chunk is a handful of `u64` ops.
- **AVX2 step path.**
  The same cascade in 256-bit lanes, 4 rows per iteration.
  Selected automatically when the build target includes AVX2.
  Other targets use the scalar fallback.
- **Edge-aware expansion.**
  A neighbor chunk is stepped only if the live chunk's edge has cells that could birth into it.
  Work scales with *active* cells, not world size.
- **WAL + atomic snapshots.**
  Per-tick batch fsync, snapshots are written off the sim task by a dedicated I/O task and truncate the WAL on success.
- **Hand-rolled binary wire protocol.**
  Little-endian, tag-prefixed frames.
- **Single render path.**
  One `drawImage` per visible chunk from an OffscreenCanvas.

