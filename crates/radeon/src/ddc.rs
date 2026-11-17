//! A monitor on a DDC line: an I2C responder at address 0x50 that serves an
//! EDID we generate. Drivers bit-bang I2C through the GPIO registers, so this
//! works one SCL edge at a time.

/// EDID 1.3 for an analog 16:10 monitor whose preferred mode is 1440x900 at
/// 60 Hz with CVT reduced blanking. It also lists 640x480, 800x600, 1024x768,
/// 1280x800, 1280x1024 and 1440x900 at 60 Hz.
pub fn edid() -> [u8; 128] {
    let mut e = [0u8; 128];
    e[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    // "SLP" as three 5-bit letters, big-endian.
    e[8..10].copy_from_slice(&[0x4d, 0x90]);
    e[10..12].copy_from_slice(&1u16.to_le_bytes());
    e[16] = 1;
    e[17] = (2002 - 1990) as u8;
    e[18..20].copy_from_slice(&[1, 3]);
    e[20] = 0x08;
    e[21..23].copy_from_slice(&[41, 26]);
    e[23] = 120;
    e[24] = 0x2a;
    e[25..35].copy_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]);
    e[35..38].copy_from_slice(&[0x21, 0x08, 0x00]);
    e[38..54].fill(0x01);
    e[38..44].copy_from_slice(&[0x81, 0x80, 0x81, 0x00, 0x95, 0x00]);
    e[54..72].copy_from_slice(&detailed_timing(&CVT_RB_1440X900, (410, 256)));
    e[72..90].copy_from_slice(&[
        0, 0, 0, 0xfd, 0, 50, 85, 30, 80, 14, 0, 0x0a, b' ', b' ', b' ', b' ', b' ', b' ',
    ]);
    e[90..95].copy_from_slice(&[0, 0, 0, 0xfc, 0]);
    e[95..108].copy_from_slice(b"86slopper\n   ");
    e[108..113].copy_from_slice(&[0, 0, 0, 0x10, 0]);
    e[127] = 0u8.wrapping_sub(e[..127].iter().fold(0u8, |a, &b| a.wrapping_add(b)));
    e
}

/// A mode's timing: active, front porch, sync and back porch, per axis.
pub struct Timing {
    pub clock_khz: u32,
    pub h: [u32; 4],
    pub v: [u32; 4],
    pub hsync_positive: bool,
    pub vsync_positive: bool,
}

/// 1440x900 at 59.9 Hz from VESA's CVT 1.1 reduced-blanking formula.
pub const CVT_RB_1440X900: Timing = Timing {
    clock_khz: 88_750,
    h: [1440, 48, 32, 80],
    v: [900, 3, 6, 17],
    hsync_positive: true,
    vsync_positive: false,
};

/// An EDID detailed timing descriptor, for a picture `size_mm` wide and high.
pub fn detailed_timing(t: &Timing, size_mm: (u32, u32)) -> [u8; 18] {
    let [ha, hf, hs, hb] = t.h;
    let [va, vf, vs, vb] = t.v;
    let (hblank, vblank) = (hf + hs + hb, vf + vs + vb);
    let clock = (t.clock_khz / 10) as u16;
    let hi = |a: u32, b: u32| ((a >> 8) << 4 | (b >> 8)) as u8;
    let flags = 0x18 | (t.vsync_positive as u8) << 2 | (t.hsync_positive as u8) << 1;
    [
        clock as u8,
        (clock >> 8) as u8,
        ha as u8,
        hblank as u8,
        hi(ha, hblank),
        va as u8,
        vblank as u8,
        hi(va, vblank),
        hf as u8,
        hs as u8,
        ((vf & 15) << 4 | (vs & 15)) as u8,
        ((hf >> 8) << 6 | (hs >> 8) << 4 | (vf >> 4) << 2 | (vs >> 4)) as u8,
        size_mm.0 as u8,
        size_mm.1 as u8,
        hi(size_mm.0, size_mm.1),
        0,
        0,
        flags,
    ]
}

/// The monitor's EDID EEPROM on the DDC bus.
pub const EDID_ADDRESS: u8 = 0x50;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, snapshot::Snap)]
enum State {
    #[default]
    Idle,
    Address,
    Write,
    Read,
    /// Addressed to someone else, or the master stopped reading.
    Ignore,
}

#[derive(snapshot::Snap)]
pub struct Ddc {
    #[snap(skip)]
    edid: [u8; 128],
    state: State,
    scl: bool,
    sda: bool,
    /// The responder holds SDA low.
    hold: bool,
    /// SCL rising edges in the current byte, 0..=9.
    clocks: u8,
    shift: u8,
    offset: u8,
    reading_next: bool,
}

impl Default for Ddc {
    fn default() -> Self {
        Ddc {
            edid: edid(),
            state: State::Idle,
            scl: true,
            sda: true,
            hold: false,
            clocks: 0,
            shift: 0,
            offset: 0,
            reading_next: false,
        }
    }
}

impl Ddc {
    pub fn edid(&self) -> &[u8; 128] {
        &self.edid
    }

    /// Level of SDA, which the master and the responder both pull down.
    pub fn sda_line(&self) -> bool {
        self.sda && !self.hold
    }

    fn present_bit(&mut self) {
        let byte = self.edid[self.offset as usize % self.edid.len()];
        self.hold = byte >> (7 - self.clocks) & 1 == 0;
    }

    /// The master drives SCL and SDA to these levels (true is released, high).
    pub fn drive(&mut self, scl: bool, sda: bool) {
        let (old_scl, old_sda) = (self.scl, self.sda);
        self.scl = scl;
        self.sda = sda;
        if scl && old_scl && old_sda != sda {
            self.hold = false;
            self.clocks = 0;
            self.shift = 0;
            self.state = if sda { State::Idle } else { State::Address };
            return;
        }
        if scl && !old_scl {
            self.clocks += 1;
            match self.state {
                State::Address | State::Write if self.clocks <= 8 => {
                    self.shift = self.shift << 1 | self.sda as u8
                }
                State::Read if self.clocks == 9 && self.sda_line() => self.state = State::Ignore,
                _ => {}
            }
        } else if !scl && old_scl {
            self.falling_edge();
        }
    }

    fn falling_edge(&mut self) {
        match self.state {
            State::Address | State::Write if self.clocks == 8 => {
                if self.state == State::Address {
                    if self.shift >> 1 == EDID_ADDRESS {
                        self.reading_next = self.shift & 1 != 0;
                        self.hold = true;
                    } else {
                        self.state = State::Ignore;
                    }
                } else {
                    self.offset = self.shift;
                    self.hold = true;
                }
            }
            State::Address | State::Write if self.clocks == 9 => {
                self.hold = false;
                self.clocks = 0;
                self.shift = 0;
                if self.state == State::Address {
                    self.state = if self.reading_next {
                        State::Read
                    } else {
                        State::Write
                    };
                    if self.state == State::Read {
                        self.present_bit();
                    }
                }
            }
            State::Read if self.clocks < 8 => self.present_bit(),
            State::Read if self.clocks == 8 => self.hold = false,
            State::Read if self.clocks == 9 => {
                self.offset = self.offset.wrapping_add(1);
                self.clocks = 0;
                self.present_bit();
            }
            _ => {}
        }
    }
}
