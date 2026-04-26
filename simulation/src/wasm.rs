//! Browser bindings for the simulation. Built only with `--features wasm`.

use crate::World;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct WasmWorld(World);

#[wasm_bindgen]
impl WasmWorld {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self(World::new())
    }

    pub fn tick(&mut self) {
        self.0.tick();
    }

    pub fn tick_number(&self) -> u64 {
        self.0.tick_number()
    }

    pub fn set_cell(&mut self, x: i64, y: i64, alive: bool) {
        self.0.set_cell(x, y, alive);
    }

    pub fn live_count(&self) -> u32 {
        self.0.iter_chunks().map(|(_, c)| c.live_count()).sum()
    }
}

impl Default for WasmWorld {
    fn default() -> Self {
        Self::new()
    }
}
