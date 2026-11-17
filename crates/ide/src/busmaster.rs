//! SFF-8038i bus master IDE registers and the PRD table walker.

use crate::DmaMemory;
use crate::channel::{Channel, DmaReq};

const CMD_START: u8 = 0x01;
const CMD_TO_MEMORY: u8 = 0x08;

const ST_ACTIVE: u8 = 0x01;
const ST_ERROR: u8 = 0x02;
const ST_INTR: u8 = 0x04;
const ST_DRIVE_CAPS: u8 = 0x60;

const PRD_EOT: u32 = 1 << 31;

#[derive(Default, snapshot::Snap)]
pub struct BusMaster {
    cmd: u8,
    status: u8,
    prd: u32,
}

impl BusMaster {
    pub fn started(&self) -> bool {
        self.cmd & CMD_START != 0
    }

    /// The interrupt bit latches on a rising edge of the channel's INTRQ.
    pub fn irq_edge(&mut self) {
        self.status |= ST_INTR;
    }
}

pub enum DmaOutcome {
    Done,
    /// The PRD table ended before the device transferred everything.
    ShortTable,
    Io,
}

impl Channel {
    pub fn bm_read(&self, off: usize) -> u8 {
        match off {
            0 => self.bm.cmd,
            2 => self.bm.status,
            4..=7 => (self.bm.prd >> (8 * (off - 4))) as u8,
            _ => 0,
        }
    }

    pub fn bm_write(&mut self, off: usize, v: u8, mem: &mut dyn DmaMemory) {
        match off {
            0 => {
                let was = self.bm.cmd;
                self.bm.cmd = v & (CMD_START | CMD_TO_MEMORY);
                if v & CMD_START == 0 {
                    self.bm.status &= !ST_ACTIVE;
                } else if was & CMD_START == 0 {
                    self.bm.status |= ST_ACTIVE;
                    if self.dma.is_some() {
                        self.run_dma(mem);
                    }
                }
            }
            2 => {
                self.bm.status &= !(v & (ST_ERROR | ST_INTR));
                self.bm.status = (self.bm.status & !ST_DRIVE_CAPS) | (v & ST_DRIVE_CAPS);
            }
            4..=7 => {
                let shift = 8 * (off - 4);
                self.bm.prd = (self.bm.prd & !(0xff << shift)) | ((v as u32) << shift);
                self.bm.prd &= !3;
            }
            _ => {}
        }
    }

    /// Runs the pending DMA request to completion through the PRD table.
    pub fn run_dma(&mut self, mem: &mut dyn DmaMemory) {
        let Some((slot, req)) = self.dma.take() else { return };
        let total = req.len();
        let mut done = 0usize;
        let mut prd = self.bm.prd;
        let mut table_used_up = false;
        let mut outcome = DmaOutcome::Done;
        let mut chunk = Vec::new();
        while done < total {
            let mut entry = [0u8; 8];
            mem.read_phys(prd, &mut entry);
            let addr = u32::from_le_bytes(entry[0..4].try_into().unwrap()) & !1;
            let flags = u32::from_le_bytes(entry[4..8].try_into().unwrap());
            let len = match flags & 0xfffe {
                0 => 0x1_0000,
                n => n as usize,
            };
            let n = len.min(total - done);
            chunk.resize(n, 0);
            if req.to_memory() {
                if self.dma_source(slot, &req, done, &mut chunk).is_err() {
                    outcome = DmaOutcome::Io;
                    break;
                }
                mem.write_phys(addr, &chunk);
            } else {
                mem.read_phys(addr, &mut chunk);
                if self.dma_sink(slot, &req, done, &chunk).is_err() {
                    outcome = DmaOutcome::Io;
                    break;
                }
            }
            done += n;
            let eot = flags & PRD_EOT != 0;
            if eot {
                table_used_up = n == len;
                if done < total {
                    outcome = DmaOutcome::ShortTable;
                }
                break;
            }
            prd = prd.wrapping_add(8);
        }
        match outcome {
            DmaOutcome::Done if !table_used_up => {}
            DmaOutcome::Done => self.bm.status &= !ST_ACTIVE,
            _ => self.bm.status = (self.bm.status & !ST_ACTIVE) | ST_ERROR,
        }
        let saved = self.sel;
        self.sel = slot;
        self.dma_finished(req, done, outcome);
        self.sel = saved;
    }

    fn dma_source(&mut self, slot: usize, req: &DmaReq, pos: usize, out: &mut [u8]) -> std::io::Result<()> {
        let d = &mut self.dev[slot];
        match req {
            DmaReq::AtaRead { lba, .. } => {
                let b = d.backend().ok_or(std::io::ErrorKind::NotFound)?;
                b.read_at(lba * 512 + pos as u64, out)
            }
            DmaReq::AtapiIn { stream, .. } => d.read_stream(stream, pos, out),
            _ => unreachable!(),
        }
    }

    fn dma_sink(&mut self, slot: usize, req: &DmaReq, pos: usize, data: &[u8]) -> std::io::Result<()> {
        match req {
            DmaReq::AtaWrite { lba, .. } => {
                let b = self.dev[slot].backend().ok_or(std::io::ErrorKind::NotFound)?;
                b.write_at(lba * 512 + pos as u64, data)
            }
            DmaReq::AtapiOut { .. } => Ok(()),
            _ => unreachable!(),
        }
    }

    fn dma_finished(&mut self, req: DmaReq, done: usize, outcome: DmaOutcome) {
        match req {
            DmaReq::AtaRead { lba, .. } | DmaReq::AtaWrite { lba, .. } => self.ata_dma_finished(lba, done, outcome),
            DmaReq::AtapiIn { .. } | DmaReq::AtapiOut { .. } => self.atapi_dma_finished(outcome),
        }
    }
}
