//! The Intel 82801AA (ICH) AC'97 audio controller, PCI 8086:2415, with one
//! codec on its AC-link.
//!
//! Register behaviour follows QEMU's hw/audio/ac97.c (InnoTek Systemberatung
//! GmbH, from VirtualBox OSE; GPL-2.0-or-later) and the 82801AA datasheet,
//! section 5.17. The bus masters move data when [`Ac97::run`] is called:
//! PCM out as fast as the host output takes it, the two inputs at their
//! sample rate in host time, filled with silence.

pub mod codec;

pub use codec::Codec;

pub const PCI_VENDOR: u16 = 0x8086;
pub const PCI_DEVICE: u16 = 0x2415;
/// Multimedia device, audio.
pub const PCI_CLASS_CODE: u32 = 0x04_01_00;
/// Size of the mixer (NAM) I/O BAR, BAR 0.
pub const NAM_LEN: u16 = 256;
/// Size of the bus master (NABM) I/O BAR, BAR 1.
pub const NABM_LEN: u16 = 64;

/// Guest physical memory as seen by the bus masters.
pub trait DmaMemory {
    fn read_phys(&mut self, addr: u32, buf: &mut [u8]);
    fn write_phys(&mut self, addr: u32, buf: &[u8]);
}

/// Where PCM out goes. It sets the pace of playback: the controller only
/// fetches as many frames as `room` allows.
pub trait AudioOut: Send {
    /// Stereo frames at `rate` Hz the output can take now.
    fn room(&mut self, rate: u32) -> usize;
    fn push(&mut self, rate: u32, frames: &[[i16; 2]]);
}

/// Throws PCM out away as soon as it arrives.
pub struct NullOut;

impl AudioOut for NullOut {
    fn room(&mut self, rate: u32) -> usize {
        rate as usize / 100
    }

    fn push(&mut self, _rate: u32, _frames: &[[i16; 2]]) {}
}

/// Status register bits.
pub mod sr {
    pub const DCH: u16 = 1;
    pub const CELV: u16 = 2;
    pub const LVBCI: u16 = 4;
    pub const BCIS: u16 = 8;
    pub const FIFOE: u16 = 16;
}

/// Control register bits.
pub mod cr {
    pub const RPBM: u8 = 1;
    pub const RR: u8 = 2;
    pub const LVBIE: u8 = 4;
    pub const FEIE: u8 = 8;
    pub const IOCE: u8 = 16;
}

/// Buffer descriptor control bits.
pub const BD_IOC: u32 = 1 << 31;
pub const BD_BUP: u32 = 1 << 30;

/// Bus master register offsets within a channel's block.
pub mod bm {
    pub const BDBAR: u16 = 0x00;
    pub const CIV: u16 = 0x04;
    pub const LVI: u16 = 0x05;
    pub const SR: u16 = 0x06;
    pub const PICB: u16 = 0x08;
    pub const PIV: u16 = 0x0a;
    pub const CR: u16 = 0x0b;
}

/// Channel blocks in the NABM BAR.
pub const PCM_IN: u16 = 0x00;
pub const PCM_OUT: u16 = 0x10;
pub const MIC_IN: u16 = 0x20;
pub const GLOB_CNT: u16 = 0x2c;
pub const GLOB_STA: u16 = 0x30;
pub const CAS: u16 = 0x34;

const GC_CR: u32 = 2;
const GC_WR: u32 = 4;
const GC_VALID: u32 = 0x3f;
const GS_S0CR: u32 = 1 << 8;
const GS_INT: [u32; 3] = [1 << 5, 1 << 6, 1 << 7];
const GS_WCLEAR: u32 = 1 << 15 | 1 << 11 | 1 << 10 | 1;
const GS_WRITABLE: u32 = 1 << 17 | 1 << 16;

const SR_WCLEAR: u16 = sr::FIFOE | sr::BCIS | sr::LVBCI;
const CR_KEPT_ON_RESET: u8 = cr::IOCE | cr::FEIE | cr::LVBIE;
const BDL_ENTRIES: u8 = 32;
const CHUNK: usize = 4096;

#[derive(Default, snapshot::Snap)]
struct Channel {
    bdbar: u32,
    civ: u8,
    lvi: u8,
    sr: u16,
    picb: u16,
    piv: u8,
    cr: u8,
    bd_valid: bool,
    bd_addr: u32,
    bd_ctl: u32,
}

impl Channel {
    fn new() -> Self {
        Channel { sr: sr::DCH, ..Default::default() }
    }

    fn reset(&mut self) {
        *self = Channel { cr: self.cr & CR_KEPT_ON_RESET, ..Channel::new() };
    }

    fn running(&self) -> bool {
        self.cr & cr::RPBM != 0 && self.sr & sr::DCH == 0
    }

    fn irq(&self) -> bool {
        (self.sr & sr::LVBCI != 0 && self.cr & cr::LVBIE != 0)
            || (self.sr & sr::BCIS != 0 && self.cr & cr::IOCE != 0)
            || (self.sr & sr::FIFOE != 0 && self.cr & cr::FEIE != 0)
    }

    fn image(&self) -> [u8; 12] {
        let mut b = [0; 12];
        b[0..4].copy_from_slice(&self.bdbar.to_le_bytes());
        b[4] = self.civ;
        b[5] = self.lvi;
        b[6..8].copy_from_slice(&self.sr.to_le_bytes());
        b[8..10].copy_from_slice(&self.picb.to_le_bytes());
        b[10] = self.piv;
        b[11] = self.cr;
        b
    }

    fn fetch(&mut self, mem: &mut dyn DmaMemory) {
        let mut b = [0; 8];
        mem.read_phys(self.bdbar.wrapping_add(self.civ as u32 * 8), &mut b);
        self.bd_valid = true;
        self.bd_addr = u32::from_le_bytes(b[0..4].try_into().unwrap()) & !1;
        self.bd_ctl = u32::from_le_bytes(b[4..8].try_into().unwrap());
        self.picb = self.bd_ctl as u16;
    }

    fn advance(&mut self, mem: &mut dyn DmaMemory) {
        self.civ = self.piv;
        self.piv = (self.piv + 1) % BDL_ENTRIES;
        self.fetch(mem);
    }

    /// Moves up to `budget` bytes through the buffer descriptor list. `data`
    /// gets each guest buffer piece as (address, length). Returns the bytes moved.
    fn transfer(&mut self, mut budget: u32, mem: &mut dyn DmaMemory, data: &mut dyn FnMut(&mut dyn DmaMemory, u32, u32)) -> u32 {
        let mut moved = 0;
        for _ in 0..2 * BDL_ENTRIES {
            if !self.running() || budget < 2 {
                break;
            }
            if !self.bd_valid {
                self.fetch(mem);
            }
            if self.picb == 0 {
                if self.civ == self.lvi {
                    self.sr |= sr::DCH;
                    break;
                }
                self.sr &= !sr::CELV;
                self.advance(mem);
                continue;
            }
            let n = (self.picb as u32 * 2).min(budget & !1);
            data(mem, self.bd_addr, n);
            self.bd_addr = self.bd_addr.wrapping_add(n);
            self.picb -= (n / 2) as u16;
            budget -= n;
            moved += n;
            if self.picb == 0 {
                let mut new_sr = self.sr & !sr::CELV;
                if self.bd_ctl & BD_IOC != 0 {
                    new_sr |= sr::BCIS;
                }
                if self.civ == self.lvi {
                    new_sr |= sr::LVBCI | sr::DCH | sr::CELV;
                } else {
                    self.advance(mem);
                }
                self.sr = new_sr;
            }
        }
        moved
    }
}

#[derive(snapshot::Snap)]
pub struct Ac97 {
    pub codec: Codec,
    /// PCM in, PCM out, mic in.
    ch: [Channel; 3],
    glob_cnt: u32,
    glob_sta: u32,
    cas: bool,
    #[snap(skip)]
    out: Box<dyn AudioOut>,
    /// Byte of a PCM out sample split across two buffers.
    carry: Vec<u8>,
    /// Host time each input channel has been filled up to.
    input_ns: [Option<u64>; 2],
}

impl Ac97 {
    pub fn new(out: Box<dyn AudioOut>) -> Self {
        Ac97 {
            codec: Codec::default(),
            ch: [Channel::new(), Channel::new(), Channel::new()],
            glob_cnt: 0,
            glob_sta: 0,
            cas: false,
            out,
            carry: Vec::new(),
            input_ns: [None; 2],
        }
    }

    /// Power-on reset. The codec is reset too: QEMU found that XP's driver
    /// reads the codec's vendor ID before it resets the codec itself.
    pub fn reset(&mut self) {
        for c in &mut self.ch {
            c.reset();
        }
        self.codec.reset();
        self.glob_cnt = 0;
        self.glob_sta = 0;
        self.cas = false;
        self.carry.clear();
    }

    /// The PCM out channel and codec state, for debugging.
    pub fn describe(&self) -> String {
        let c = &self.ch[1];
        format!(
            "ac97: po civ {} lvi {} piv {} sr {:#x} cr {:#x} picb {} bdbar {:#x} bd {:#x}/{:#x} | rate {} gain {:?} glob_cnt {:#x} irq {}",
            c.civ, c.lvi, c.piv, c.sr, c.cr, c.picb, c.bdbar, c.bd_addr, c.bd_ctl,
            self.codec.dac_rate(), self.codec.out_gain(), self.glob_cnt, self.irq()
        )
    }

    /// Level of INTA#.
    pub fn irq(&self) -> bool {
        self.ch.iter().any(Channel::irq)
    }

    pub fn nam_read(&mut self, off: u16, size: u8) -> u32 {
        self.cas = false;
        match size {
            2 => self.codec.read(off as u8) as u32,
            _ => u32::MAX >> (32 - 8 * size as u32),
        }
    }

    pub fn nam_write(&mut self, off: u16, size: u8, v: u32) {
        self.cas = false;
        if size == 2 {
            self.codec.write(off as u8, v as u16);
        }
    }

    fn glob_sta(&self) -> u32 {
        let ints = (0..3).filter(|&i| self.ch[i].irq()).fold(0, |a, i| a | GS_INT[i]);
        self.glob_sta | GS_S0CR | ints
    }

    fn nabm_byte(&mut self, off: u16) -> u8 {
        match off {
            GLOB_CNT..=0x2f => (self.glob_cnt >> (8 * (off - GLOB_CNT))) as u8,
            GLOB_STA..=0x33 => (self.glob_sta() >> (8 * (off - GLOB_STA))) as u8,
            CAS => std::mem::replace(&mut self.cas, true) as u8,
            0..GLOB_CNT if off & 0xf < 12 => self.ch[(off >> 4) as usize].image()[(off & 0xf) as usize],
            _ => 0xff,
        }
    }

    pub fn nabm_read(&mut self, off: u16, size: u8) -> u32 {
        (0..size as u16).fold(0, |v, i| v | (self.nabm_byte(off + i) as u32) << (8 * i))
    }

    pub fn nabm_write(&mut self, off: u16, size: u8, v: u32, mem: &mut dyn DmaMemory) {
        let c = &mut self.ch[(off >> 4).min(2) as usize];
        let reg = if off < GLOB_CNT { off & 0xf } else { off };
        match (reg, size) {
            (bm::LVI, 1) => {
                if c.cr & cr::RPBM != 0 && c.sr & sr::DCH != 0 {
                    c.sr &= !(sr::DCH | sr::CELV);
                    c.advance(mem);
                }
                c.lvi = v as u8 % BDL_ENTRIES;
            }
            (bm::CR, 1) => {
                if v as u8 & cr::RR != 0 {
                    c.reset();
                } else {
                    c.cr = v as u8 & 0x1f;
                    if c.cr & cr::RPBM == 0 {
                        c.sr |= sr::DCH;
                    } else {
                        c.advance(mem);
                        c.sr &= !sr::DCH;
                    }
                }
                if off >> 4 != 1 {
                    self.input_ns[(off >> 5) as usize] = None;
                }
            }
            (bm::SR, 1 | 2) => c.sr &= !(v as u16 & SR_WCLEAR),
            (bm::BDBAR, 4) => c.bdbar = v & !7,
            (GLOB_CNT, 4) => {
                if v & (GC_WR | GC_CR) == 0 {
                    self.glob_cnt = v & GC_VALID;
                }
            }
            (GLOB_STA, 4) => {
                self.glob_sta &= !(v & GS_WCLEAR);
                self.glob_sta = (self.glob_sta & !GS_WRITABLE) | (v & GS_WRITABLE);
            }
            _ => {}
        }
    }

    /// Runs the bus masters up to host time `now_ns`.
    pub fn run(&mut self, now_ns: u64, mem: &mut dyn DmaMemory) {
        self.run_output(mem);
        for (slot, i) in [(0, 0), (1, 2)] {
            let rate = if i == 0 { self.codec.adc_rate() } else { self.codec.mic_rate() } as u64;
            if !self.ch[i].running() || rate == 0 {
                self.input_ns[slot] = None;
                continue;
            }
            let since = *self.input_ns[slot].get_or_insert(now_ns);
            let frames = (now_ns - since) * rate / 1_000_000_000;
            if frames == 0 {
                continue;
            }
            self.input_ns[slot] = Some(since + frames * 1_000_000_000 / rate);
            let frame_bytes = if i == 0 { 4 } else { 2 };
            let zeros = [0u8; CHUNK];
            let budget = (frames * frame_bytes).min(u32::MAX as u64) as u32;
            self.ch[i].transfer(budget, mem, &mut |mem, addr, len| {
                for at in (0..len).step_by(CHUNK) {
                    mem.write_phys(addr + at, &zeros[..(len - at).min(CHUNK as u32) as usize]);
                }
            });
        }
    }

    fn run_output(&mut self, mem: &mut dyn DmaMemory) {
        let rate = self.codec.dac_rate();
        if !self.ch[1].running() || rate == 0 {
            return;
        }
        let room = self.out.room(rate) as u32;
        let gain = self.codec.out_gain();
        let (out, carry) = (&mut self.out, &mut self.carry);
        let mut buf = [0u8; CHUNK];
        let mut frames = Vec::with_capacity(CHUNK / 4 + 1);
        let budget = (room * 4).saturating_sub(carry.len() as u32);
        self.ch[1].transfer(budget, mem, &mut |mem, addr, len| {
            for at in (0..len).step_by(CHUNK) {
                let n = (len - at).min(CHUNK as u32) as usize;
                mem.read_phys(addr + at, &mut buf[..n]);
                carry.extend_from_slice(&buf[..n]);
                let whole = carry.len() & !3;
                frames.clear();
                frames.extend(carry[..whole].as_chunks::<4>().0.iter().map(|f| {
                    let s = |i: usize| {
                        let v = i16::from_le_bytes([f[2 * i], f[2 * i + 1]]) as f32 * gain[i];
                        v.clamp(i16::MIN as f32, i16::MAX as f32) as i16
                    };
                    [s(0), s(1)]
                }));
                carry.drain(..whole);
                out.push(rate, &frames);
            }
        });
    }
}

#[cfg(test)]
mod tests;
