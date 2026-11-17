//! 8254 programmable interval timer (Intel 82C54 datasheet). Counters run
//! from a nanosecond clock supplied by the board.

pub const FREQ_HZ: u64 = 1_193_182;

fn ticks_at(ns: u64) -> u64 {
    (ns as u128 * FREQ_HZ as u128 / 1_000_000_000) as u64
}

fn ns_at(ticks: u64) -> u64 {
    (ticks as u128 * 1_000_000_000).div_ceil(FREQ_HZ as u128) as u64
}

#[derive(Clone, Copy, Default, PartialEq, Eq, snapshot::Snap)]
enum Access {
    #[default]
    Word,
    Lo,
    Hi,
}

#[derive(Clone, Default, snapshot::Snap)]
struct Channel {
    mode: u8,
    access: Access,
    bcd: bool,
    reload: u32,
    /// Tick at which the current count was loaded; `None` until programmed.
    start: Option<u64>,
    gate: bool,
    latched: Option<u16>,
    status_latched: Option<u8>,
    write_hi_next: bool,
    read_hi_next: bool,
    write_lsb: u8,
    null_count: bool,
}

impl Channel {
    fn period(&self) -> u64 {
        if self.reload == 0 { 0x10000 } else { self.reload as u64 }
    }

    /// Ticks elapsed since the count started, or `None` if not counting.
    fn elapsed(&self, now: u64) -> Option<u64> {
        let start = self.start?;
        if !self.gate && matches!(self.mode, 0 | 2 | 3 | 4) {
            return None;
        }
        Some(now.saturating_sub(start))
    }

    fn count(&self, now: u64) -> u16 {
        let Some(e) = self.elapsed(now) else { return self.reload as u16 };
        let p = self.period();
        match self.mode {
            0 | 1 | 4 | 5 => (p.wrapping_sub(e) & 0xffff) as u16,
            2 => (p - e % p) as u16,
            _ => {
                let half = e % p;
                ((p - (2 * half) % p) & 0xfffe) as u16
            }
        }
    }

    fn out(&self, now: u64) -> bool {
        let Some(e) = self.elapsed(now) else {
            return self.mode != 0;
        };
        let p = self.period();
        match self.mode {
            0 | 1 => e >= p,
            2 => e % p != p - 1,
            3 => e % p < p.div_ceil(2),
            _ => e != p,
        }
    }

    /// Tick of the next rising edge of OUT after `now`, for modes that make one.
    fn next_edge(&self, now: u64) -> Option<u64> {
        let start = self.start?;
        if !self.gate {
            return None;
        }
        let p = self.period();
        let e = now.saturating_sub(start);
        match self.mode {
            0 => (e < p).then_some(start + p),
            4 => (e <= p).then_some(start + p + 1),
            2 => Some(start + (e / p + 1) * p),
            3 => Some(start + (e / p + 1) * p),
            _ => None,
        }
    }
}

#[derive(snapshot::Snap)]
pub struct Pit {
    ch: [Channel; 3],
    /// Tick up to which channel 0 edges have been reported.
    irq_seen: u64,
    port61: u8,
}

/// Timer ticks remembered for catch-up; beyond this, time is lost.
pub const MAX_BACKLOG: u32 = 1000;

impl Default for Pit {
    fn default() -> Self {
        let mut ch: [Channel; 3] = Default::default();
        ch[0].gate = true;
        ch[1].gate = true;
        Pit { ch, irq_seen: 0, port61: 0 }
    }
}

impl Pit {
    /// Number of channel-0 rising edges between the last poll and `now_ns`.
    /// Channel-0 edges since the last call, capped at one second's worth.
    pub fn poll_irq0(&mut self, now_ns: u64) -> u32 {
        let now = ticks_at(now_ns);
        let mut edges = 0;
        while let Some(edge) = self.ch[0].next_edge(self.irq_seen) {
            if edge > now {
                break;
            }
            self.irq_seen = edge;
            edges += 1;
            if matches!(self.ch[0].mode, 0 | 4) || edges >= MAX_BACKLOG {
                self.irq_seen = now;
                break;
            }
        }
        if edges == 0 && self.ch[0].next_edge(self.irq_seen).is_none() {
            self.irq_seen = now;
        }
        edges
    }

    /// Nanoseconds until the next channel-0 edge, if one is scheduled.
    pub fn next_irq0_ns(&self, now_ns: u64) -> Option<u64> {
        let now = ticks_at(now_ns);
        let edge = self.ch[0].next_edge(now.max(self.irq_seen))?;
        Some(ns_at(edge).saturating_sub(now_ns))
    }

    pub fn read(&mut self, port: u16, now_ns: u64) -> u8 {
        let now = ticks_at(now_ns);
        if port == 0x61 {
            let refresh = ((now_ns / 15_085) & 1) as u8;
            let out2 = self.ch[2].out(now) as u8;
            return (self.port61 & 0x0f) | refresh << 4 | out2 << 5;
        }
        let c = &mut self.ch[(port & 3) as usize];
        if let Some(s) = c.status_latched.take() {
            return s;
        }
        let value = c.latched.unwrap_or_else(|| c.count(now));
        let byte = match c.access {
            Access::Lo => {
                c.latched = None;
                value as u8
            }
            Access::Hi => {
                c.latched = None;
                (value >> 8) as u8
            }
            Access::Word => {
                let hi = c.read_hi_next;
                c.read_hi_next = !hi;
                if hi {
                    c.latched = None;
                    (value >> 8) as u8
                } else {
                    if c.latched.is_none() {
                        c.latched = Some(value);
                    }
                    value as u8
                }
            }
        };
        byte
    }

    pub fn write(&mut self, port: u16, v: u8, now_ns: u64) {
        let now = ticks_at(now_ns);
        match port {
            0x61 => {
                let gate = v & 1 != 0;
                let c = &mut self.ch[2];
                if gate && !c.gate && c.start.is_some() {
                    c.start = Some(now);
                }
                c.gate = gate;
                self.port61 = v & 0x0f;
            }
            0x43 => self.control(v, now),
            _ => {
                let idx = (port & 3) as usize;
                let c = &mut self.ch[idx];
                let load = match c.access {
                    Access::Lo => Some(v as u32),
                    Access::Hi => Some((v as u32) << 8),
                    Access::Word => {
                        if c.write_hi_next {
                            c.write_hi_next = false;
                            Some(c.write_lsb as u32 | (v as u32) << 8)
                        } else {
                            c.write_lsb = v;
                            c.write_hi_next = true;
                            None
                        }
                    }
                };
                if let Some(n) = load {
                    c.reload = n;
                    c.start = Some(now);
                    c.null_count = false;
                    if idx == 0 {
                        self.irq_seen = now;
                    }
                }
            }
        }
    }

    fn control(&mut self, v: u8, now: u64) {
        let sel = v >> 6;
        if sel == 3 {
            for i in 0..3 {
                if v & (2 << i) == 0 {
                    continue;
                }
                let c = &mut self.ch[i];
                if v & 0x20 == 0 && c.latched.is_none() {
                    c.latched = Some(c.count(now));
                }
                if v & 0x10 == 0 && c.status_latched.is_none() {
                    let access = match c.access {
                        Access::Lo => 1,
                        Access::Hi => 2,
                        Access::Word => 3,
                    };
                    c.status_latched = Some(
                        (c.out(now) as u8) << 7 | (c.null_count as u8) << 6 | access << 4 | c.mode << 1 | c.bcd as u8,
                    );
                }
            }
            return;
        }
        let c = &mut self.ch[sel as usize];
        let rw = (v >> 4) & 3;
        if rw == 0 {
            if c.latched.is_none() {
                c.latched = Some(c.count(now));
            }
            return;
        }
        c.access = match rw {
            1 => Access::Lo,
            2 => Access::Hi,
            _ => Access::Word,
        };
        c.mode = match (v >> 1) & 7 {
            6 => 2,
            7 => 3,
            m => m,
        };
        c.bcd = v & 1 != 0;
        c.start = None;
        c.latched = None;
        c.write_hi_next = false;
        c.read_hi_next = false;
        c.null_count = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode2_counts_down_and_raises_periodic_edges() {
        let mut p = Pit::default();
        p.write(0x43, 0x34, 0);
        p.write(0x40, 0x9c, 0);
        p.write(0x40, 0x2e, 0);
        let one_period_ns = ns_at(0x2e9c);
        assert_eq!(p.poll_irq0(one_period_ns / 2), 0);
        assert_eq!(p.poll_irq0(one_period_ns + 1000), 1);
        assert_eq!(p.poll_irq0(3 * one_period_ns + 1000), 2);
    }

    #[test]
    fn mode4_strobe_raises_one_edge_after_terminal_count() {
        let mut p = Pit::default();
        p.write(0x43, 0x38, 0);
        p.write(0x40, 0x00, 0);
        p.write(0x40, 0x10, 0);
        assert_eq!(p.poll_irq0(ns_at(0xfff)), 0);
        assert!(p.next_irq0_ns(ns_at(0xfff)).is_some());
        assert_eq!(p.poll_irq0(ns_at(0x1002)), 1);
        assert_eq!(p.poll_irq0(ns_at(0x3000)), 0);
        assert_eq!(p.next_irq0_ns(ns_at(0x3000)), None);
    }

    #[test]
    fn latch_freezes_count() {
        let mut p = Pit::default();
        p.write(0x43, 0x34, 0);
        p.write(0x40, 0x00, 0);
        p.write(0x40, 0x10, 0);
        p.write(0x43, 0x00, ns_at(0x100));
        let lo = p.read(0x40, ns_at(0x800));
        let hi = p.read(0x40, ns_at(0x800));
        assert_eq!(u16::from_le_bytes([lo, hi]), 0x1000 - 0x100);
    }

    #[test]
    fn channel2_out_goes_high_after_terminal_count() {
        let mut p = Pit::default();
        p.write(0x61, 0x01, 0);
        p.write(0x43, 0xb0, 0);
        p.write(0x42, 0x10, 0);
        p.write(0x42, 0x00, 0);
        assert_eq!(p.read(0x61, ns_at(8)) & 0x20, 0);
        assert_ne!(p.read(0x61, ns_at(0x11)) & 0x20, 0);
    }
}
