//! UHCI host controller (Intel UHCI Design Guide, revision 1.1).
//!
//! Register behaviour and the schedule walker follow QEMU's
//! hw/usb/hcd-uhci.c (Copyright (c) 2005 Fabrice Bellard, (c) 2008 Max
//! Krasnyansky, MIT license), minus its asynchronous packet handling: our
//! devices always answer a transaction straight away.

use crate::{DmaMemory, Pid, Reply, UsbDevice};

pub const PORTS: usize = 2;

const FRAME_NS: u64 = 1_000_000;
/// After a long stall, frames older than this are skipped instead of replayed.
const MAX_FRAMES_BEHIND: u64 = 128;
const MAX_FRAMES_PER_RUN: u64 = 16;
const FRAME_MAX_LOOPS: usize = 256;
const MAX_QUEUES: usize = 128;
/// Full-speed bandwidth per frame, in bytes.
const FRAME_BANDWIDTH: usize = 1280;

const USBCMD: u16 = 0x00;
const USBSTS: u16 = 0x02;
const USBINTR: u16 = 0x04;
const USBFRNUM: u16 = 0x06;
const USBFLBASEADD: u16 = 0x08;
const USBFLBASEADD_HI: u16 = 0x0a;
const USBSOF: u16 = 0x0c;
const USBPORTSC1: u16 = 0x10;
const USBPORTSC2: u16 = 0x12;

const CMD_RS: u16 = 1 << 0;
const CMD_HCRESET: u16 = 1 << 1;
const CMD_GRESET: u16 = 1 << 2;
const CMD_EGSM: u16 = 1 << 3;
const CMD_FGR: u16 = 1 << 4;

const STS_USBINT: u16 = 1 << 0;
const STS_USBERR: u16 = 1 << 1;
const STS_RD: u16 = 1 << 2;
const STS_HSERR: u16 = 1 << 3;
const STS_HCPERR: u16 = 1 << 4;
const STS_HCHALTED: u16 = 1 << 5;

const INTR_TIMEOUT_CRC: u16 = 1 << 0;
const INTR_RESUME: u16 = 1 << 1;
const INTR_IOC: u16 = 1 << 2;
const INTR_SPD: u16 = 1 << 3;

/// Causes of a USBINT interrupt, kept apart because USBSTS has one bit for both.
const INT_IOC: u8 = 1 << 0;
const INT_SPD: u8 = 1 << 1;

const PORT_CCS: u16 = 1 << 0;
const PORT_CSC: u16 = 1 << 1;
const PORT_EN: u16 = 1 << 2;
const PORT_ENC: u16 = 1 << 3;
const PORT_RD: u16 = 1 << 6;
/// Reserved, always reads as 1.
const PORT_RSVD1: u16 = 1 << 7;
const PORT_RESET: u16 = 1 << 9;
const PORT_SUSPEND: u16 = 1 << 12;
const PORT_READ_ONLY: u16 = 0x1bb;
const PORT_WRITE_CLEAR: u16 = PORT_CSC | PORT_ENC;

const TD_ACTLEN: u32 = 0x7ff;
const TD_TIMEOUT: u32 = 1 << 18;
const TD_NAK: u32 = 1 << 19;
const TD_STALL: u32 = 1 << 22;
const TD_ACTIVE: u32 = 1 << 23;
const TD_IOC: u32 = 1 << 24;
const TD_SPD: u32 = 1 << 29;
const TD_ERROR_COUNT: u32 = 3 << 27;

const LINK_TERMINATE: u32 = 1 << 0;
const LINK_QH: u32 = 1 << 1;
const LINK_DEPTH_FIRST: u32 = 1 << 2;

const PID_SETUP: u8 = 0x2d;
const PID_IN: u8 = 0x69;
const PID_OUT: u8 = 0xe1;

#[derive(Default, Clone, Copy)]
struct Td {
    link: u32,
    ctrl: u32,
    token: u32,
    buffer: u32,
}

enum TdResult {
    StopFrame,
    NextQh,
    Complete,
}

struct Port {
    sc: u16,
    dev: Option<Box<dyn UsbDevice>>,
}

pub struct Uhci {
    cmd: u16,
    status: u16,
    /// Pending USBINT causes (`INT_*`).
    int_causes: u8,
    intr: u16,
    frnum: u16,
    flbase: u32,
    sof: u8,
    ports: [Port; PORTS],
    /// Host time at which the next frame is due, while the schedule runs.
    next_frame: Option<u64>,
    /// Frames run since power-on.
    frames: u64,
}

impl Default for Uhci {
    fn default() -> Self {
        Self::new()
    }
}

impl Uhci {
    pub fn new() -> Self {
        let mut u = Uhci {
            cmd: 0,
            status: 0,
            int_causes: 0,
            intr: 0,
            frnum: 0,
            flbase: 0,
            sof: 0,
            ports: [Port { sc: 0, dev: None }, Port { sc: 0, dev: None }],
            next_frame: None,
            frames: 0,
        };
        u.reset();
        u
    }

    /// Plugs `dev` into root port `port` (0-based).
    pub fn attach(&mut self, port: usize, dev: Box<dyn UsbDevice>) {
        let p = &mut self.ports[port];
        p.dev = Some(dev);
        p.sc |= PORT_CCS | PORT_CSC;
    }

    pub fn device<T: UsbDevice>(&self, port: usize) -> Option<&T> {
        let dev: &dyn std::any::Any = self.ports[port].dev.as_deref()?;
        dev.downcast_ref()
    }

    pub fn device_mut<T: UsbDevice>(&mut self, port: usize) -> Option<&mut T> {
        let dev: &mut dyn std::any::Any = self.ports[port].dev.as_deref_mut()?;
        dev.downcast_mut()
    }

    /// Whether the host is running the schedule.
    pub fn running(&self) -> bool {
        self.cmd & CMD_RS != 0
    }

    /// Frames run since power-on, one per millisecond while running.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Hardware reset (also HCRESET and GRESET).
    pub fn reset(&mut self) {
        self.cmd = 0;
        self.status = STS_HCHALTED;
        self.int_causes = 0;
        self.intr = 0;
        self.frnum = 0;
        self.flbase = 0;
        self.sof = 64;
        self.next_frame = None;
        for p in &mut self.ports {
            p.sc = PORT_RSVD1;
            if let Some(d) = &mut p.dev {
                d.reset();
                p.sc |= PORT_CCS | PORT_CSC;
            }
        }
    }

    /// Level of the controller's interrupt line.
    pub fn irq(&self) -> bool {
        (self.int_causes & INT_IOC != 0 && self.intr & INTR_IOC != 0)
            || (self.int_causes & INT_SPD != 0 && self.intr & INTR_SPD != 0)
            || (self.status & STS_USBERR != 0 && self.intr & INTR_TIMEOUT_CRC != 0)
            || (self.status & STS_RD != 0 && self.intr & INTR_RESUME != 0)
            || self.status & (STS_HSERR | STS_HCPERR) != 0
    }

    /// Host time of the next frame, if the schedule is running.
    pub fn next_frame_ns(&self) -> Option<u64> {
        self.next_frame
    }

    /// `off` is relative to the I/O BAR. Byte and dword accesses are split
    /// into the 16-bit registers like QEMU's memory core does.
    pub fn read(&self, off: u16, size: u8) -> u32 {
        match size {
            1 => (self.read16(off & !1) >> (8 * (off & 1))) as u32 & 0xff,
            2 => self.read16(off) as u32,
            _ => self.read16(off) as u32 | (self.read16(off + 2) as u32) << 16,
        }
    }

    pub fn write(&mut self, off: u16, size: u8, v: u32, now: u64) {
        match size {
            1 => self.write16(off & !1, ((v & 0xff) << (8 * (off & 1))) as u16, now),
            2 => self.write16(off, v as u16, now),
            _ => {
                self.write16(off, v as u16, now);
                self.write16(off + 2, (v >> 16) as u16, now);
            }
        }
    }

    fn read16(&self, off: u16) -> u16 {
        match off {
            USBCMD => self.cmd,
            USBSTS => self.status,
            USBINTR => self.intr,
            USBFRNUM => self.frnum,
            USBFLBASEADD => self.flbase as u16,
            USBFLBASEADD_HI => (self.flbase >> 16) as u16,
            USBSOF => self.sof as u16,
            USBPORTSC1 | USBPORTSC2 => self.ports[(off - USBPORTSC1) as usize / 2].sc,
            _ => 0xff7f,
        }
    }

    fn write16(&mut self, off: u16, v: u16, now: u64) {
        match off {
            USBCMD => {
                if v & CMD_RS != 0 && self.cmd & CMD_RS == 0 {
                    self.next_frame = Some(now + FRAME_NS);
                    self.status &= !STS_HCHALTED;
                } else if v & CMD_RS == 0 {
                    self.status |= STS_HCHALTED;
                    self.next_frame = None;
                }
                if v & (CMD_GRESET | CMD_HCRESET) != 0 {
                    self.reset();
                    return;
                }
                self.cmd = v;
                if v & CMD_EGSM != 0 && self.ports.iter().any(|p| p.sc & PORT_RD != 0) {
                    self.resume();
                }
            }
            USBSTS => {
                self.status &= !v;
                if v & STS_USBINT != 0 {
                    self.int_causes = 0;
                }
            }
            USBINTR => self.intr = v & 0xf,
            USBFRNUM => {
                if self.status & STS_HCHALTED != 0 {
                    self.frnum = v & 0x7ff;
                }
            }
            USBFLBASEADD => self.flbase = (self.flbase & 0xffff_0000) | (v as u32 & !0xfff),
            USBFLBASEADD_HI => self.flbase = (self.flbase & 0xffff) | (v as u32) << 16,
            USBSOF => self.sof = v as u8,
            USBPORTSC1 | USBPORTSC2 => {
                let p = &mut self.ports[(off - USBPORTSC1) as usize / 2];
                let mut v = v;
                if let Some(d) = &mut p.dev
                    && v & PORT_RESET != 0
                    && p.sc & PORT_RESET == 0
                {
                    d.reset();
                }
                p.sc &= PORT_READ_ONLY;
                if p.sc & PORT_CCS == 0 {
                    v &= !PORT_EN;
                }
                p.sc |= v & !PORT_READ_ONLY;
                p.sc &= !(v & PORT_WRITE_CLEAR);
            }
            _ => {}
        }
    }

    fn resume(&mut self) {
        if self.cmd & CMD_EGSM != 0 {
            self.cmd |= CMD_FGR;
            self.status |= STS_RD;
        }
    }

    /// Runs every frame that is due by host time `now`.
    pub fn run(&mut self, now: u64, mem: &mut dyn DmaMemory) {
        for i in 0..PORTS {
            let p = &mut self.ports[i];
            if let Some(d) = &mut p.dev
                && d.take_wakeup()
                && p.sc & PORT_SUSPEND != 0
                && p.sc & PORT_RD == 0
            {
                p.sc |= PORT_RD;
                self.resume();
            }
        }
        let Some(mut due) = self.next_frame else { return };
        if now < due {
            return;
        }
        let mut frames = (now - due) / FRAME_NS + 1;
        if frames > MAX_FRAMES_BEHIND {
            let skipped = frames - MAX_FRAMES_BEHIND;
            due += skipped * FRAME_NS;
            self.frnum = (self.frnum.wrapping_add(skipped as u16)) & 0x7ff;
            frames = MAX_FRAMES_BEHIND;
        }
        let mut int_mask = 0;
        for _ in 0..frames.min(MAX_FRAMES_PER_RUN) {
            self.frames += 1;
            for d in self.ports.iter_mut().filter_map(|p| p.dev.as_mut()) {
                d.start_frame(self.frames);
            }
            int_mask |= self.process_frame(mem);
            // FRNUM moves on as soon as a frame is done, so on an interrupt the
            // guest looks at FRNUM - 1, as on real hardware.
            self.frnum = (self.frnum + 1) & 0x7ff;
            due += FRAME_NS;
            if self.cmd & CMD_RS == 0 {
                self.status |= STS_HCHALTED;
                self.next_frame = None;
                break;
            }
            self.next_frame = Some(due);
        }
        if int_mask != 0 {
            self.int_causes |= int_mask;
            self.status |= STS_USBINT;
        }
    }

    fn find_device(&mut self, addr: u8) -> Option<&mut Box<dyn UsbDevice>> {
        self.ports
            .iter_mut()
            .filter(|p| p.sc & PORT_EN != 0)
            .filter_map(|p| p.dev.as_mut())
            .find(|d| d.address() == addr)
    }

    /// Walks one frame's schedule and returns the USBINT causes it raised.
    fn process_frame(&mut self, mem: &mut dyn DmaMemory) -> u8 {
        let mut link = read32(mem, self.flbase + ((self.frnum as u32 & 0x3ff) << 2));
        let mut int_mask = 0;
        let mut curr_qh = 0u32;
        let mut qh = (0u32, 0u32);
        let mut seen = Vec::<u32>::new();
        let mut td_count = 0;
        let mut frame_bytes = 0usize;
        for _ in 0..FRAME_MAX_LOOPS {
            if link & LINK_TERMINATE != 0 || frame_bytes >= FRAME_BANDWIDTH {
                break;
            }
            if link & LINK_QH != 0 {
                if seen.contains(&link) || seen.len() >= MAX_QUEUES {
                    // Going around a reclamation loop is fine as long as the
                    // last lap did some work.
                    if td_count == 0 {
                        break;
                    }
                    td_count = 0;
                    seen.clear();
                }
                seen.push(link);
                let addr = link & !0xf;
                qh = (read32(mem, addr), read32(mem, addr + 4));
                if qh.1 & LINK_TERMINATE != 0 {
                    curr_qh = 0;
                    link = qh.0;
                } else {
                    curr_qh = link;
                    link = qh.1;
                }
                continue;
            }

            let addr = link & !0xf;
            let mut td = Td {
                link: read32(mem, addr),
                ctrl: read32(mem, addr + 4),
                token: read32(mem, addr + 8),
                buffer: read32(mem, addr + 12),
            };
            let old_ctrl = td.ctrl;
            let result = self.handle_td(&mut td, mem, &mut int_mask);
            if td.ctrl != old_ctrl {
                mem.write_phys(addr + 4, &td.ctrl.to_le_bytes());
            }
            match result {
                TdResult::StopFrame => break,
                TdResult::NextQh => link = if curr_qh != 0 { qh.0 } else { td.link },
                TdResult::Complete => {
                    link = td.link;
                    td_count += 1;
                    frame_bytes += (td.ctrl & TD_ACTLEN).wrapping_add(1) as usize & 0x7ff;
                    if curr_qh != 0 {
                        qh.1 = link;
                        mem.write_phys((curr_qh & !0xf) + 4, &link.to_le_bytes());
                        if link & LINK_DEPTH_FIRST == 0 {
                            curr_qh = 0;
                            link = qh.0;
                        }
                    }
                }
            }
        }
        int_mask
    }

    fn handle_td(&mut self, td: &mut Td, mem: &mut dyn DmaMemory, int_mask: &mut u8) -> TdResult {
        if td.ctrl & TD_ACTIVE == 0 {
            // UHCI 1.1, 3.2.2: IOC fires even for a TD that was already inactive.
            if td.ctrl & TD_IOC != 0 {
                *int_mask |= INT_IOC;
            }
            return TdResult::NextQh;
        }
        let ep = ((td.token >> 15) & 0xf) as u8;
        let pid = match td.token as u8 {
            PID_SETUP if ep == 0 => Pid::Setup,
            PID_IN => Pid::In,
            PID_OUT => Pid::Out,
            _ => {
                self.status |= STS_HCPERR;
                self.cmd &= !CMD_RS;
                return TdResult::StopFrame;
            }
        };
        let max_len = ((td.token >> 21) + 1) as usize & 0x7ff;
        let mut buf = [0u8; 0x800];
        let buf = &mut buf[..max_len];
        if pid != Pid::In {
            mem.read_phys(td.buffer, buf);
        }
        let Some(dev) = self.find_device(((td.token >> 8) & 0x7f) as u8) else {
            td.ctrl |= TD_TIMEOUT;
            td.ctrl &= !TD_ERROR_COUNT;
            return self.td_error(td, int_mask);
        };
        let len = match dev.transact(pid, ep, buf) {
            Reply::Nak => {
                td.ctrl |= TD_NAK;
                return TdResult::NextQh;
            }
            Reply::Stall => {
                td.ctrl |= TD_STALL;
                return self.td_error(td, int_mask);
            }
            Reply::Ack(n) if pid == Pid::In => n.min(max_len),
            Reply::Ack(_) => max_len,
        };
        td.ctrl = (td.ctrl & !TD_ACTLEN) | (len as u32).wrapping_sub(1) & TD_ACTLEN;
        // The NAK bit may be left over from an earlier frame. QEMU notes that
        // Windows 2000 relies on it being cleared here.
        td.ctrl &= !(TD_ACTIVE | TD_NAK);
        if td.ctrl & TD_IOC != 0 {
            *int_mask |= INT_IOC;
        }
        if pid == Pid::In {
            mem.write_phys(td.buffer, &buf[..len]);
            if td.ctrl & TD_SPD != 0 && len < max_len {
                // Short packet: the queue stops here and the QH is not advanced.
                *int_mask |= INT_SPD;
                return TdResult::NextQh;
            }
        }
        TdResult::Complete
    }

    fn td_error(&mut self, td: &mut Td, int_mask: &mut u8) -> TdResult {
        td.ctrl &= !TD_ACTIVE;
        self.status |= STS_USBERR;
        if td.ctrl & TD_IOC != 0 {
            *int_mask |= INT_IOC;
        }
        TdResult::NextQh
    }
}

fn read32(mem: &mut dyn DmaMemory, addr: u32) -> u32 {
    let mut b = [0; 4];
    mem.read_phys(addr, &mut b);
    u32::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeRam(Vec<u8>);

    impl DmaMemory for FakeRam {
        fn read_phys(&mut self, addr: u32, buf: &mut [u8]) {
            let a = addr as usize;
            buf.copy_from_slice(&self.0[a..a + buf.len()]);
        }

        fn write_phys(&mut self, addr: u32, buf: &[u8]) {
            let a = addr as usize;
            self.0[a..a + buf.len()].copy_from_slice(buf);
        }
    }

    impl FakeRam {
        fn put(&mut self, addr: u32, words: &[u32]) {
            for (i, w) in words.iter().enumerate() {
                self.write_phys(addr + 4 * i as u32, &w.to_le_bytes());
            }
        }

        fn word(&mut self, addr: u32) -> u32 {
            read32(self, addr)
        }
    }

    /// Answers SETUP and OUT with an ACK and IN with a fixed payload, or NAKs everything.
    struct Echo {
        addr: u8,
        nak: bool,
        resets: u32,
        setups: Vec<Vec<u8>>,
    }

    impl UsbDevice for Echo {
        fn reset(&mut self) {
            self.resets += 1;
        }

        fn address(&self) -> u8 {
            self.addr
        }

        fn transact(&mut self, pid: Pid, _ep: u8, buf: &mut [u8]) -> Reply {
            if self.nak {
                return Reply::Nak;
            }
            match pid {
                Pid::Setup => {
                    self.setups.push(buf.to_vec());
                    Reply::Ack(buf.len())
                }
                Pid::Out => Reply::Ack(buf.len()),
                Pid::In => {
                    let data = [0x12, 0x01, 0x10, 0x01];
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Reply::Ack(n)
                }
            }
        }
    }

    const FL: u32 = 0x1000;
    const QH: u32 = 0x2000;
    const TD0: u32 = 0x2100;
    const TD1: u32 = 0x2120;
    const SETUP_BUF: u32 = 0x3000;
    const IN_BUF: u32 = 0x3100;

    fn token(pid: u8, addr: u8, ep: u8, len: u32) -> u32 {
        ((len.wrapping_sub(1) & 0x7ff) << 21) | (ep as u32) << 15 | (addr as u32) << 8 | pid as u32
    }

    fn setup(nak: bool) -> (Uhci, FakeRam) {
        let mut ram = FakeRam(vec![0; 0x10000]);
        for f in 0..1024 {
            ram.put(FL + 4 * f, &[QH | LINK_QH]);
        }
        ram.put(QH, &[LINK_TERMINATE, TD0]);
        ram.put(TD0, &[TD1 | LINK_DEPTH_FIRST, TD_ACTIVE | 3 << 27, token(PID_SETUP, 0, 0, 8), SETUP_BUF]);
        ram.put(TD1, &[LINK_TERMINATE, TD_ACTIVE | TD_IOC | TD_SPD, token(PID_IN, 0, 0, 8), IN_BUF]);
        ram.put(SETUP_BUF, &[0x0100_0680, 0x0012_0000]);
        let mut u = Uhci::new();
        u.attach(0, Box::new(Echo { addr: 0, nak, resets: 0, setups: Vec::new() }));
        u.write(USBPORTSC1, 2, PORT_EN as u32, 0);
        u.write(USBFLBASEADD, 4, FL, 0);
        u.write(USBINTR, 2, 0xf, 0);
        u.write(USBCMD, 2, CMD_RS as u32, 0);
        (u, ram)
    }

    #[test]
    fn runs_a_control_transfer_through_a_qh() {
        let (mut u, mut ram) = setup(false);
        assert!(!u.irq());
        u.run(FRAME_NS, &mut ram);
        assert_eq!(u.read(USBFRNUM, 2), 1);
        let dev = u.device_mut::<Echo>(0).unwrap();
        assert_eq!(dev.setups, vec![vec![0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]]);
        assert_eq!(ram.word(TD0 + 4) & (TD_ACTIVE | TD_ACTLEN), 7);
        let in_ctrl = ram.word(TD1 + 4);
        assert_eq!(in_ctrl & TD_ACTIVE, 0);
        assert_eq!(in_ctrl & TD_ACTLEN, 3);
        assert_eq!(ram.word(IN_BUF), 0x0110_0112);
        // The SETUP TD completed, so the QH moved on to the IN TD. The IN TD was
        // short, so the QH still points at it.
        assert_eq!(ram.word(QH + 4), TD1 | LINK_DEPTH_FIRST);
        assert_eq!(u.read(USBSTS, 2) as u16 & STS_USBINT, STS_USBINT);
        assert!(u.irq());
        u.write(USBSTS, 2, STS_USBINT as u32, FRAME_NS);
        assert!(!u.irq());
    }

    #[test]
    fn nak_leaves_the_td_active() {
        let (mut u, mut ram) = setup(true);
        u.run(FRAME_NS, &mut ram);
        let ctrl = ram.word(TD0 + 4);
        assert_eq!(ctrl & (TD_ACTIVE | TD_NAK), TD_ACTIVE | TD_NAK);
        assert_eq!(ram.word(QH + 4), TD0);
        assert!(!u.irq());
    }

    #[test]
    fn missing_device_times_out() {
        let (mut u, mut ram) = setup(false);
        ram.put(TD0 + 8, &[token(PID_SETUP, 5, 0, 8)]);
        u.run(FRAME_NS, &mut ram);
        let ctrl = ram.word(TD0 + 4);
        assert_eq!(ctrl & (TD_ACTIVE | TD_TIMEOUT | TD_ERROR_COUNT), TD_TIMEOUT);
        assert_ne!(u.read(USBSTS, 2) as u16 & STS_USBERR, 0);
        assert!(u.irq());
    }

    #[test]
    fn disabled_port_hides_its_device() {
        let (mut u, mut ram) = setup(false);
        u.write(USBPORTSC1, 2, 0, 0);
        u.run(FRAME_NS, &mut ram);
        assert_ne!(ram.word(TD0 + 4) & TD_TIMEOUT, 0);
    }

    #[test]
    fn frames_follow_host_time() {
        let (mut u, mut ram) = setup(true);
        u.run(FRAME_NS - 1, &mut ram);
        assert_eq!(u.read(USBFRNUM, 2), 0);
        u.run(5 * FRAME_NS, &mut ram);
        assert_eq!(u.read(USBFRNUM, 2), 5);
        assert_eq!(u.next_frame_ns(), Some(6 * FRAME_NS));
        u.write(USBCMD, 2, 0, 5 * FRAME_NS);
        assert_ne!(u.read(USBSTS, 2) as u16 & STS_HCHALTED, 0);
        u.run(100 * FRAME_NS, &mut ram);
        assert_eq!(u.read(USBFRNUM, 2), 5);
    }

    #[test]
    fn port_status_and_reset() {
        let mut u = Uhci::new();
        assert_eq!(u.read(USBPORTSC1, 2), PORT_RSVD1 as u32);
        assert_eq!(u.read(USBPORTSC1 + 2, 2), PORT_RSVD1 as u32);
        u.write(USBPORTSC1, 2, PORT_EN as u32, 0);
        assert_eq!(u.read(USBPORTSC1, 2) as u16 & PORT_EN, 0);
        u.attach(0, Box::new(Echo { addr: 0, nak: false, resets: 0, setups: Vec::new() }));
        let sc = u.read(USBPORTSC1, 2) as u16;
        assert_eq!(sc, PORT_RSVD1 | PORT_CCS | PORT_CSC);
        u.write(USBPORTSC1, 2, (PORT_RESET | PORT_CSC) as u32, 0);
        assert_eq!(u.read(USBPORTSC1, 2) as u16, PORT_RSVD1 | PORT_CCS | PORT_RESET);
        assert_eq!(u.device_mut::<Echo>(0).unwrap().resets, 1);
        u.write(USBPORTSC1, 2, 0, 0);
        u.write(USBPORTSC1, 2, PORT_EN as u32, 0);
        assert_eq!(u.read(USBPORTSC1, 2) as u16, PORT_RSVD1 | PORT_CCS | PORT_EN);
        assert_eq!(u.read(0x14, 2), 0xff7f);
    }

    #[test]
    fn byte_and_dword_access() {
        let mut u = Uhci::new();
        u.write(USBFLBASEADD, 4, 0x1234_5678, 0);
        assert_eq!(u.read(USBFLBASEADD, 4), 0x1234_5000);
        assert_eq!(u.read(USBSOF, 1), 64);
        u.write(USBSOF, 1, 0x50, 0);
        assert_eq!(u.read(USBSOF, 1), 0x50);
        assert_eq!(u.read(USBSTS, 1), STS_HCHALTED as u32);
    }
}
