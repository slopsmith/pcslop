//! A USB HID tablet: an absolute pointer with three buttons and a wheel.
//!
//! The descriptors and report format are those of QEMU's usb-tablet
//! (hw/usb/dev-hid.c and hw/input/hid.c, Copyright (c) 2005 Fabrice Bellard,
//! (c) 2007 OpenMoko, Inc., MIT license), so guests that work with
//! `-device usb-tablet` see the same device.

use std::collections::VecDeque;

use crate::control::{ControlHandler, ControlPipe, Setup};
use crate::{Pid, Reply, UsbDevice};

/// Largest coordinate the tablet reports on either axis.
pub const MAX_COORD: u16 = 0x7fff;

const QUEUE_LEN: usize = 16;
/// The guest counts as using the tablet if it polled within this many frames.
const IN_USE_FRAMES: u64 = 100;

const STR_MANUFACTURER: u8 = 1;
const STR_PRODUCT: u8 = 2;
const STR_SERIAL: u8 = 3;
const STR_CONFIG: u8 = 4;

#[rustfmt::skip]
const DEVICE_DESCRIPTOR: [u8; 18] = [
    18, 0x01,
    0x00, 0x01,         // bcdUSB 1.0
    0x00, 0x00, 0x00,   // class, subclass and protocol come from the interface
    8,                  // bMaxPacketSize0
    0x27, 0x06,         // idVendor and idProduct: QEMU's usb-tablet
    0x01, 0x00,
    0x00, 0x00,         // bcdDevice
    STR_MANUFACTURER, STR_PRODUCT, STR_SERIAL,
    1,                  // bNumConfigurations
];

#[rustfmt::skip]
const HID_DESCRIPTOR: [u8; 9] = [
    9, 0x21,
    0x01, 0x00,         // bcdHID
    0x00,               // bCountryCode
    1,
    0x22, REPORT_DESCRIPTOR.len() as u8, 0,
];

const CONFIG_LEN: usize = 9 + 9 + 9 + 7;

#[rustfmt::skip]
const CONFIG_DESCRIPTOR: [u8; CONFIG_LEN] = {
    let mut d = [
        // configuration
        9, 0x02, CONFIG_LEN as u8, 0,
        1,                  // bNumInterfaces
        1,                  // bConfigurationValue
        STR_CONFIG,
        0xa0,               // bus powered, remote wakeup
        50,                 // 100 mA
        // interface 0: HID, no boot protocol
        9, 0x04, 0, 0, 1, 0x03, 0x00, 0x00, 0,
        // HID descriptor, filled in below
        0, 0, 0, 0, 0, 0, 0, 0, 0,
        // endpoint 1 IN, interrupt, 8 bytes, every 10 ms
        7, 0x05, 0x81, 0x03, 8, 0, 10,
    ];
    let mut i = 0;
    while i < HID_DESCRIPTOR.len() {
        d[18 + i] = HID_DESCRIPTOR[i];
        i += 1;
    }
    d
};

#[rustfmt::skip]
const REPORT_DESCRIPTOR: [u8; 74] = [
    0x05, 0x01,         // Usage Page (Generic Desktop)
    0x09, 0x02,         // Usage (Mouse)
    0xa1, 0x01,         // Collection (Application)
    0x09, 0x01,         //   Usage (Pointer)
    0xa1, 0x00,         //   Collection (Physical)
    0x05, 0x09,         //     Usage Page (Button)
    0x19, 0x01,         //     Usage Minimum (1)
    0x29, 0x05,         //     Usage Maximum (5)
    0x15, 0x00,         //     Logical Minimum (0)
    0x25, 0x01,         //     Logical Maximum (1)
    0x95, 0x05,         //     Report Count (5)
    0x75, 0x01,         //     Report Size (1)
    0x81, 0x02,         //     Input (Data, Variable, Absolute)
    0x95, 0x01,         //     Report Count (1)
    0x75, 0x03,         //     Report Size (3)
    0x81, 0x01,         //     Input (Constant)
    0x05, 0x01,         //     Usage Page (Generic Desktop)
    0x09, 0x30,         //     Usage (X)
    0x09, 0x31,         //     Usage (Y)
    0x15, 0x00,         //     Logical Minimum (0)
    0x26, 0xff, 0x7f,   //     Logical Maximum (0x7fff)
    0x35, 0x00,         //     Physical Minimum (0)
    0x46, 0xff, 0x7f,   //     Physical Maximum (0x7fff)
    0x75, 0x10,         //     Report Size (16)
    0x95, 0x02,         //     Report Count (2)
    0x81, 0x02,         //     Input (Data, Variable, Absolute)
    0x05, 0x01,         //     Usage Page (Generic Desktop)
    0x09, 0x38,         //     Usage (Wheel)
    0x15, 0x81,         //     Logical Minimum (-0x7f)
    0x25, 0x7f,         //     Logical Maximum (0x7f)
    0x35, 0x00,         //     Physical Minimum (same as logical)
    0x45, 0x00,         //     Physical Maximum (same as logical)
    0x75, 0x08,         //     Report Size (8)
    0x95, 0x01,         //     Report Count (1)
    0x81, 0x06,         //     Input (Data, Variable, Relative)
    0xc0,               //   End Collection
    0xc0,               // End Collection
];

const REPORT_LEN: usize = 6;

fn string(index: u8) -> Option<&'static str> {
    Some(match index {
        STR_MANUFACTURER => "pcslop",
        STR_PRODUCT => "pcslop USB Tablet",
        STR_SERIAL => "1",
        STR_CONFIG => "HID Tablet",
        _ => return None,
    })
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, snapshot::Snap)]
struct Report {
    x: u16,
    y: u16,
    buttons: u8,
    wheel: i32,
}

impl Report {
    fn bytes(&self) -> [u8; REPORT_LEN] {
        let [xl, xh] = self.x.to_le_bytes();
        let [yl, yh] = self.y.to_le_bytes();
        [self.buttons, xl, xh, yl, yh, self.wheel.clamp(-127, 127) as i8 as u8]
    }
}

#[derive(Default, snapshot::Snap)]
struct State {
    address: u8,
    config: u8,
    remote_wakeup: bool,
    /// HID idle rate in units of 4 ms; 0 reports only on change.
    idle: u8,
    next_idle: u64,
    frame: u64,
    last_poll: Option<u64>,
    /// Reports not yet sent, oldest first.
    queue: VecDeque<Report>,
    /// The last report sent, minus its wheel motion.
    current: Report,
    wakeup: bool,
}

impl State {
    fn report_now(&mut self) -> Report {
        let r = self.queue.pop_front().unwrap_or(self.current);
        self.current = Report { wheel: 0, ..r };
        r
    }
}

fn put(data: &mut [u8], src: &[u8]) -> Option<usize> {
    let n = src.len().min(data.len());
    data[..n].copy_from_slice(&src[..n]);
    Some(n)
}

fn string_descriptor(index: u8, data: &mut [u8]) -> Option<usize> {
    if index == 0 {
        return put(data, &[4, 0x03, 0x09, 0x04]);
    }
    let s = string(index)?;
    let mut d = vec![0, 0x03];
    d.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
    d[0] = d.len() as u8;
    put(data, &d)
}

impl ControlHandler for State {
    fn control(&mut self, s: &Setup, data: &mut [u8]) -> Option<usize> {
        let [index, kind] = s.value.to_le_bytes();
        if s.request_type & 0x1f == 1 && s.index != 0 {
            return None;
        }
        match (s.request_type, s.request) {
            (0x80, 0x06) => match kind {
                0x01 => put(data, &DEVICE_DESCRIPTOR),
                0x02 if index == 0 => put(data, &CONFIG_DESCRIPTOR),
                0x03 => string_descriptor(index, data),
                _ => None,
            },
            (0x81, 0x06) => match kind {
                0x21 => put(data, &HID_DESCRIPTOR),
                0x22 => put(data, &REPORT_DESCRIPTOR),
                _ => None,
            },
            (0x00, 0x05) => {
                self.address = (s.value & 0x7f) as u8;
                Some(0)
            }
            (0x80, 0x08) => put(data, &[self.config]),
            (0x00, 0x09) if s.value <= 1 => {
                self.config = s.value as u8;
                Some(0)
            }
            (0x80, 0x00) => put(data, &[(self.remote_wakeup as u8) << 1, 0]),
            (0x81 | 0x82, 0x00) => put(data, &[0, 0]),
            (0x00, 0x01 | 0x03) if s.value == 1 => {
                self.remote_wakeup = s.request == 0x03;
                Some(0)
            }
            // ENDPOINT_HALT: the interrupt endpoint never halts, so there is nothing to clear.
            (0x02, 0x01 | 0x03) if s.value == 0 => Some(0),
            (0x81, 0x0a) => put(data, &[0]),
            (0x01, 0x0b) if s.value == 0 => Some(0),
            // HID class requests (HID 1.11, 7.2).
            (0xa1, 0x01) => {
                let r = self.report_now();
                put(data, &r.bytes())
            }
            (0xa1, 0x02) => put(data, &[self.idle]),
            (0x21, 0x0a) => {
                self.idle = (s.value >> 8) as u8;
                self.next_idle = self.frame + 4 * self.idle as u64;
                Some(0)
            }
            _ => None,
        }
    }
}

#[derive(Default, snapshot::Snap)]
pub struct Tablet {
    pipe: ControlPipe,
    st: State,
}

impl Tablet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves the pointer to (`x`, `y`), each 0..=`MAX_COORD`, with `buttons`
    /// (bit 0 left, 1 right, 2 middle) held. `wheel` counts detents away from
    /// the user.
    pub fn input(&mut self, x: u16, y: u16, buttons: u8, wheel: i32) {
        let new = Report { x: x.min(MAX_COORD), y: y.min(MAX_COORD), buttons: buttons & 7, wheel };
        let q = &mut self.st.queue;
        let full = q.len() >= QUEUE_LEN;
        match q.back_mut() {
            Some(back) if back.buttons == new.buttons || full => {
                *back = Report { wheel: back.wheel + wheel, ..new };
            }
            None if new == self.st.current => return,
            _ => q.push_back(new),
        }
        if self.st.remote_wakeup {
            self.st.wakeup = true;
        }
    }

    /// Whether the guest has configured the tablet and is polling it.
    pub fn in_use(&self) -> bool {
        self.st.config != 0 && self.st.last_poll.is_some_and(|f| self.st.frame - f < IN_USE_FRAMES)
    }

    fn interrupt_in(&mut self, buf: &mut [u8]) -> Reply {
        let st = &mut self.st;
        st.last_poll = Some(st.frame);
        let idle_due = st.idle != 0 && st.frame >= st.next_idle;
        if st.queue.is_empty() && !idle_due {
            return Reply::Nak;
        }
        st.next_idle = st.frame + 4 * st.idle as u64;
        let r = st.report_now().bytes();
        let n = r.len().min(buf.len());
        buf[..n].copy_from_slice(&r[..n]);
        Reply::Ack(n)
    }
}

impl UsbDevice for Tablet {
    fn reset(&mut self) {
        self.pipe.reset();
        let st = &mut self.st;
        st.address = 0;
        st.config = 0;
        st.remote_wakeup = false;
        st.idle = 0;
        st.queue.clear();
        st.wakeup = false;
    }

    fn address(&self) -> u8 {
        self.st.address
    }

    fn transact(&mut self, pid: Pid, ep: u8, buf: &mut [u8]) -> Reply {
        match (ep, pid) {
            (0, _) => self.pipe.transact(&mut self.st, pid, buf),
            (1, Pid::In) => self.interrupt_in(buf),
            _ => Reply::Stall,
        }
    }

    fn start_frame(&mut self, frame: u64) {
        self.st.frame = frame;
    }

    fn take_wakeup(&mut self) -> bool {
        std::mem::take(&mut self.st.wakeup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a whole control transfer with 8-byte packets, as a host would.
    fn control(t: &mut Tablet, setup: [u8; 8]) -> Result<Vec<u8>, ()> {
        let mut s = setup;
        if t.transact(Pid::Setup, 0, &mut s) != Reply::Ack(8) {
            return Err(());
        }
        let len = u16::from_le_bytes([setup[6], setup[7]]) as usize;
        let mut out = Vec::new();
        if setup[0] & 0x80 != 0 {
            while out.len() < len {
                let mut b = [0; 8];
                match t.transact(Pid::In, 0, &mut b) {
                    Reply::Ack(n) => {
                        out.extend_from_slice(&b[..n]);
                        if n < 8 {
                            break;
                        }
                    }
                    _ => return Err(()),
                }
            }
            match t.transact(Pid::Out, 0, &mut []) {
                Reply::Ack(0) => Ok(out),
                _ => Err(()),
            }
        } else {
            match t.transact(Pid::In, 0, &mut []) {
                Reply::Ack(0) => Ok(out),
                _ => Err(()),
            }
        }
    }

    fn get_descriptor(t: &mut Tablet, rt: u8, kind: u8, index: u8, len: u16) -> Result<Vec<u8>, ()> {
        let [l0, l1] = len.to_le_bytes();
        control(t, [rt, 0x06, index, kind, 0, 0, l0, l1])
    }

    #[test]
    fn descriptors_are_consistent() {
        assert_eq!(DEVICE_DESCRIPTOR[0] as usize, DEVICE_DESCRIPTOR.len());
        let c = &CONFIG_DESCRIPTOR;
        assert_eq!(u16::from_le_bytes([c[2], c[3]]) as usize, c.len());
        let mut kinds = Vec::new();
        let mut i = 0;
        while i < c.len() {
            kinds.push(c[i + 1]);
            i += c[i] as usize;
        }
        assert_eq!(i, c.len());
        assert_eq!(kinds, [0x02, 0x04, 0x21, 0x05]);
        assert_eq!(c[18 + 7] as usize, REPORT_DESCRIPTOR.len());
    }

    #[test]
    fn report_descriptor_describes_six_bytes() {
        let d = &REPORT_DESCRIPTOR;
        let (mut size, mut count, mut bits, mut depth) = (0u32, 0u32, 0u32, 0i32);
        let mut i = 0;
        while i < d.len() {
            let prefix = d[i];
            let n = [0, 1, 2, 4][(prefix & 3) as usize];
            let v = d[i + 1..i + 1 + n].iter().rev().fold(0u32, |a, &b| a << 8 | b as u32);
            match prefix & 0xfc {
                0x74 => size = v,
                0x94 => count = v,
                0x80 => bits += size * count,
                0xa0 => depth += 1,
                0xc0 => depth -= 1,
                _ => {}
            }
            i += 1 + n;
        }
        assert_eq!(depth, 0);
        assert_eq!(bits as usize, 8 * REPORT_LEN);
    }

    #[test]
    fn enumerates_like_a_host_would() {
        let mut t = Tablet::new();
        let dev = get_descriptor(&mut t, 0x80, 1, 0, 64).unwrap();
        assert_eq!(dev, DEVICE_DESCRIPTOR);

        control(&mut t, [0x00, 0x05, 3, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(t.address(), 3);

        let head = get_descriptor(&mut t, 0x80, 2, 0, 9).unwrap();
        assert_eq!(head.len(), 9);
        let total = u16::from_le_bytes([head[2], head[3]]);
        assert_eq!(get_descriptor(&mut t, 0x80, 2, 0, total).unwrap(), CONFIG_DESCRIPTOR);

        assert_eq!(get_descriptor(&mut t, 0x80, 3, 0, 255).unwrap(), [4, 3, 0x09, 0x04]);
        let product = get_descriptor(&mut t, 0x80, 3, STR_PRODUCT, 255).unwrap();
        let text: Vec<u16> = product[2..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16(&text).unwrap(), "pcslop USB Tablet");
        assert!(get_descriptor(&mut t, 0x80, 3, 0xee, 255).is_err());

        control(&mut t, [0x00, 0x09, 1, 0, 0, 0, 0, 0]).unwrap();
        control(&mut t, [0x21, 0x0a, 0, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(get_descriptor(&mut t, 0x81, 0x22, 0, 0x4a + 0x40).unwrap(), REPORT_DESCRIPTOR);
        assert!(control(&mut t, [0x21, 0x0b, 0, 0, 0, 0, 0, 0]).is_err());
        assert!(control(&mut t, [0x00, 0x09, 2, 0, 0, 0, 0, 0]).is_err());

        t.reset();
        assert_eq!(t.address(), 0);
    }

    #[test]
    fn interrupt_endpoint_naks_until_something_changes() {
        let mut t = Tablet::new();
        control(&mut t, [0x00, 0x09, 1, 0, 0, 0, 0, 0]).unwrap();
        let mut b = [0; 8];
        assert_eq!(t.transact(Pid::In, 1, &mut b), Reply::Nak);
        assert!(t.in_use());

        t.input(0x4000, 0x2000, 1, 0);
        t.input(0x4100, 0x2100, 1, 2);
        t.input(0x4200, 0x2200, 0, 0);
        assert_eq!(t.transact(Pid::In, 1, &mut b), Reply::Ack(6));
        assert_eq!(b[..6], [1, 0x00, 0x41, 0x00, 0x21, 2]);
        assert_eq!(t.transact(Pid::In, 1, &mut b), Reply::Ack(6));
        assert_eq!(b[..6], [0, 0x00, 0x42, 0x00, 0x22, 0]);
        assert_eq!(t.transact(Pid::In, 1, &mut b), Reply::Nak);

        t.input(0x4200, 0x2200, 0, 0);
        assert_eq!(t.transact(Pid::In, 1, &mut b), Reply::Nak);
        assert_eq!(t.transact(Pid::Out, 1, &mut b), Reply::Stall);
    }

    #[test]
    fn idle_rate_repeats_the_last_report() {
        let mut t = Tablet::new();
        control(&mut t, [0x21, 0x0a, 0, 2, 0, 0, 0, 0]).unwrap();
        let mut b = [0; 8];
        assert_eq!(t.transact(Pid::In, 1, &mut b), Reply::Nak);
        t.start_frame(8);
        assert_eq!(t.transact(Pid::In, 1, &mut b), Reply::Ack(6));
        assert_eq!(t.transact(Pid::In, 1, &mut b), Reply::Nak);
    }
}
