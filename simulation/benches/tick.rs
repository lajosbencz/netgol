//! Tick throughput benchmarks. Four workloads exercise different regimes:
//!   - `glider`        single glider, mostly empty world (constant-cost minimum)
//!   - `r_pentomino`   chaotic growth across many chunks
//!   - `random_soup`   dense 64x64 random region (worst-case kernel pressure)
//!   - `stable_blocks` many still-life blocks (per-tick fixed-cost regime)

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use simulation::{TickOutcome, World};

fn build_glider() -> World {
    let mut w = World::new();
    for &(x, y) in &[(1i64, 0i64), (2, 1), (0, 2), (1, 2), (2, 2)] {
        w.set_cell(x, y, true);
    }
    w
}

fn build_r_pentomino() -> World {
    let mut w = World::new();
    for &(x, y) in &[(1i64, 0i64), (2, 0), (0, 1), (1, 1), (1, 2)] {
        w.set_cell(x, y, true);
    }
    w
}

fn build_random_soup() -> World {
    // 64x64 region centered around origin, ~50% density from a deterministic LCG.
    let mut w = World::new();
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for y in -32i64..32 {
        for x in -32i64..32 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            if state >> 63 == 1 {
                w.set_cell(x, y, true);
            }
        }
    }
    w
}

fn build_stable_blocks(count: usize) -> World {
    // Spread 2x2 blocks so they don't interact: stride > 4 in both axes.
    let mut w = World::new();
    let side = (count as f64).sqrt().ceil() as i64;
    let stride = 8i64;
    for j in 0..side {
        for i in 0..side {
            let bx = i * stride;
            let by = j * stride;
            w.set_cell(bx, by, true);
            w.set_cell(bx + 1, by, true);
            w.set_cell(bx, by + 1, true);
            w.set_cell(bx + 1, by + 1, true);
        }
    }
    w
}

fn run_n_ticks(world: &mut World, outcome: &mut TickOutcome, n: usize) {
    for _ in 0..n {
        world.tick_into(outcome);
    }
}

fn bench_glider(c: &mut Criterion) {
    c.bench_function("glider_1000_ticks", |b| {
        b.iter_batched(
            || (build_glider(), TickOutcome::default()),
            |(mut w, mut o)| run_n_ticks(&mut w, &mut o, 1000),
            BatchSize::SmallInput,
        );
    });
}

fn bench_r_pentomino(c: &mut Criterion) {
    c.bench_function("r_pentomino_500_ticks", |b| {
        b.iter_batched(
            || (build_r_pentomino(), TickOutcome::default()),
            |(mut w, mut o)| run_n_ticks(&mut w, &mut o, 500),
            BatchSize::SmallInput,
        );
    });
}

fn bench_random_soup(c: &mut Criterion) {
    c.bench_function("random_soup_64x64_200_ticks", |b| {
        b.iter_batched(
            || (build_random_soup(), TickOutcome::default()),
            |(mut w, mut o)| run_n_ticks(&mut w, &mut o, 200),
            BatchSize::SmallInput,
        );
    });
}

fn bench_stable_blocks(c: &mut Criterion) {
    c.bench_function("stable_blocks_1024_100_ticks", |b| {
        b.iter_batched(
            || (build_stable_blocks(1024), TickOutcome::default()),
            |(mut w, mut o)| run_n_ticks(&mut w, &mut o, 100),
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(
    benches,
    bench_glider,
    bench_r_pentomino,
    bench_random_soup,
    bench_stable_blocks,
);
criterion_main!(benches);
