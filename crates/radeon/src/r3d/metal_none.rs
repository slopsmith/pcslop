//! Stands in for the Metal backend where there is no Metal.

use super::raster::{Captured, Raster};
use super::zmask::Fill;

pub(crate) struct Pipeline;

pub(crate) struct MetalR3d;

impl MetalR3d {
    pub fn new(_vram: &mut [u8]) -> Option<Self> {
        None
    }

    pub fn prepare(&mut self, _r: &Raster, _cap: &Captured) -> Option<Pipeline> {
        None
    }

    pub fn draw(&mut self, _r: &Raster, _cap: &Captured, _p: &Pipeline) {}

    pub fn fill(&mut self, _f: Fill, _bpp: usize, _value: u32) -> bool {
        false
    }

    pub fn kick(&mut self) -> u64 {
        0
    }

    pub fn completed(&mut self) -> u64 {
        0
    }

    pub fn busy(&self) -> bool {
        false
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (0, 0, 0)
    }

    pub fn sync(&mut self) -> u32 {
        0
    }
}
