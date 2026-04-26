//! Conway's Game of Life core. Sparse chunked world, bit-parallel step.
//!
//! Two public entry points: [`Chunk`] for stepping a single chunk against a halo, and
//! [`World`] for advancing a sparse map of chunks together. Cell coordinates are `i64`
//! externally; chunk coordinates are `i32`. The world panics on any coordinate that
//! cannot be expressed in those bounds - fail early.

pub const CHUNK_SIZE: usize = 64;
pub const CHUNK_SIZE_I64: i64 = CHUNK_SIZE as i64;

mod chunk;
mod world;

#[cfg(feature = "wasm")]
mod wasm;

pub use chunk::{Chunk, EdgeBundle, FrozenMask, StepResult};
pub use world::{ChunkCoord, TickOutcome, World};
