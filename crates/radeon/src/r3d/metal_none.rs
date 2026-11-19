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

    pub fn finished(&self) -> u64 {
        0
    }

    pub fn busy(&self) -> bool {
        false
    }

    pub fn pipelines(&self) -> usize {
        0
    }

    pub fn writes_pending(&mut self, _start: usize, _end: usize) -> bool {
        false
    }

    pub fn gpu_ms(&self) -> u64 {
        0
    }

    pub fn throttles(&self) -> u64 {
        0
    }

    pub fn committed(&self) -> u64 {
        0
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (0, 0, 0)
    }

    pub fn sync(&mut self) -> u32 {
        0
    }

    pub fn vram_written(&mut self) {}

    pub fn draw_vertices(&mut self, _r: &Raster, _slots: &[usize], _vs: &super::vsgen::VsDesc, _job: &super::vsgen::VertexJob) -> bool {
        false
    }

    pub fn gpu_vertices(&self) -> bool {
        false
    }

    pub fn vertex_draws(&self) -> u64 {
        0
    }

    pub fn texture_stats(&self) -> (u64, u64, u64, u64) {
        (0, 0, 0, 0)
    }
}
