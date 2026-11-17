//! 8042 keyboard controller with a PS/2 keyboard and mouse.
//!
//! The keyboard is fed set-1 scancodes by the frontend and always reports set 1,
//! which is what the controller's translation mode produces; untranslated set 2
//! output is not implemented.

use std::collections::VecDeque;

const STATUS_OBF: u8 = 0x01;
const STATUS_SYS: u8 = 0x04;
const STATUS_CMD: u8 = 0x08;
const STATUS_UNLOCKED: u8 = 0x10;
const STATUS_AUX: u8 = 0x20;

const CB_KBD_INT: u8 = 0x01;
const CB_AUX_INT: u8 = 0x02;
const CB_SYS: u8 = 0x04;
const CB_KBD_OFF: u8 = 0x10;
const CB_AUX_OFF: u8 = 0x20;
const CB_TRANSLATE: u8 = 0x40;

#[derive(Clone, Copy, PartialEq, Eq, Default, snapshot::Snap)]
enum Src {
    #[default]
    Kbd,
    Aux,
}

#[derive(Clone, Copy, PartialEq, Eq, snapshot::Snap)]
enum Pending {
    None,
    CommandByte,
    OutputPort,
    KbdOutput,
    AuxOutput,
    AuxWrite,
    KbdArg(u8),
}

#[derive(snapshot::Snap)]
pub struct I8042 {
    command_byte: u8,
    status_cmd: bool,
    out: Option<(u8, Src)>,
    kbd_queue: VecDeque<u8>,
    aux_queue: VecDeque<u8>,
    pending: Pending,
    output_port: u8,
    kbd_enabled: bool,
    mouse: Mouse,
    pub reset_requested: bool,
}

#[derive(Default, snapshot::Snap)]
struct Mouse {
    reporting: bool,
    sample_rate: u8,
    resolution: u8,
    scaling21: bool,
    id: u8,
    rate_history: [u8; 3],
    buttons: u8,
    expect_arg: Option<u8>,
    #[snap(skip)]
    pending: Motion,
}

#[derive(Default, Clone, Copy)]
struct Motion {
    dx: i32,
    dy: i32,
    wheel: i32,
    buttons_changed: bool,
}

impl Motion {
    fn waiting(&self) -> bool {
        self.buttons_changed || self.dx != 0 || self.dy != 0 || self.wheel != 0
    }
}

const AUX_QUEUE_LIMIT: usize = 32;
const MAX_PENDING: i32 = 4096;

impl Default for I8042 {
    fn default() -> Self {
        I8042 {
            command_byte: CB_KBD_INT | CB_SYS | CB_TRANSLATE,
            status_cmd: false,
            out: None,
            kbd_queue: VecDeque::new(),
            aux_queue: VecDeque::new(),
            pending: Pending::None,
            output_port: 0x03,
            kbd_enabled: true,
            mouse: Mouse { sample_rate: 100, resolution: 2, ..Default::default() },
            reset_requested: false,
        }
    }
}

impl I8042 {
    pub fn a20(&self) -> bool {
        self.output_port & 2 != 0
    }

    /// Level of IRQ 1 (keyboard) and IRQ 12 (mouse).
    pub fn irqs(&self) -> (bool, bool) {
        match self.out {
            Some((_, Src::Kbd)) => (self.command_byte & CB_KBD_INT != 0, false),
            Some((_, Src::Aux)) => (false, self.command_byte & CB_AUX_INT != 0),
            None => (false, false),
        }
    }

    fn refill(&mut self) {
        if self.out.is_some() {
            return;
        }
        if self.command_byte & CB_KBD_OFF == 0
            && let Some(b) = self.kbd_queue.pop_front()
        {
            self.out = Some((b, Src::Kbd));
            return;
        }
        if self.command_byte & CB_AUX_OFF == 0
            && let Some(b) = self.aux_queue.pop_front()
        {
            self.out = Some((b, Src::Aux));
            if self.aux_queue.is_empty() && self.mouse.pending.waiting() {
                self.send_motion();
            }
        }
    }

    fn controller_reply(&mut self, b: u8) {
        self.kbd_queue.push_front(b);
        let saved = self.command_byte;
        self.command_byte &= !CB_KBD_OFF;
        self.refill();
        self.command_byte = saved;
    }

    fn kbd_reply(&mut self, bytes: &[u8]) {
        self.kbd_queue.extend(bytes);
        self.refill();
    }

    fn aux_reply(&mut self, bytes: &[u8]) {
        self.aux_queue.extend(bytes);
        self.refill();
    }

    /// Queues a key event. `code` is a set-1 make code, with 0xE0xx for extended keys.
    pub fn key(&mut self, code: u16, pressed: bool) {
        if !self.kbd_enabled {
            return;
        }
        let brk = if pressed { 0 } else { 0x80 };
        if code >> 8 == 0xe0 {
            self.kbd_queue.push_back(0xe0);
        }
        self.kbd_queue.push_back((code as u8) | brk);
        self.refill();
    }

    /// Queues a relative mouse movement (positive dy is up), wheel detents
    /// (positive is away from the user) and button state. Motion larger than
    /// one packet holds is split over several.
    pub fn mouse_motion(&mut self, dx: i32, dy: i32, wheel: i32, buttons: u8) {
        if !self.mouse.reporting {
            return;
        }
        let p = &mut self.mouse.pending;
        let add = |acc: i32, v: i32| acc.saturating_add(v).clamp(-MAX_PENDING, MAX_PENDING);
        p.dx = add(p.dx, dx);
        p.dy = add(p.dy, dy);
        if self.mouse.id == 3 {
            p.wheel = add(p.wheel, wheel);
        }
        if buttons & 7 != self.mouse.buttons {
            self.mouse.buttons = buttons & 7;
            p.buttons_changed = true;
        }
        self.send_motion();
    }

    fn send_motion(&mut self) {
        let size = if self.mouse.id == 3 { 4 } else { 3 };
        loop {
            let p = self.mouse.pending;
            if !self.mouse.reporting || !p.waiting() || self.aux_queue.len() + size > AUX_QUEUE_LIMIT {
                break;
            }
            let dx = p.dx.clamp(-256, 255);
            let dy = p.dy.clamp(-256, 255);
            // The IntelliMouse wheel byte counts detents towards the user.
            let dz = (-p.wheel).clamp(-8, 7);
            self.mouse.pending = Motion {
                dx: p.dx - dx,
                dy: p.dy - dy,
                wheel: p.wheel + dz,
                buttons_changed: false,
            };
            let b0 = 0x08 | self.mouse.buttons | if dx < 0 { 0x10 } else { 0 } | if dy < 0 { 0x20 } else { 0 };
            self.aux_queue.extend([b0, dx as u8, dy as u8]);
            if size == 4 {
                self.aux_queue.push_back(dz as u8);
            }
        }
        self.refill();
    }

    pub fn read(&mut self, port: u16) -> u8 {
        if port == 0x64 {
            let mut s = STATUS_UNLOCKED;
            if self.command_byte & CB_SYS != 0 {
                s |= STATUS_SYS;
            }
            if self.status_cmd {
                s |= STATUS_CMD;
            }
            match self.out {
                Some((_, Src::Kbd)) => s |= STATUS_OBF,
                Some((_, Src::Aux)) => s |= STATUS_OBF | STATUS_AUX,
                None => {}
            }
            return s;
        }
        let v = match self.out.take() {
            Some((b, _)) => b,
            None => 0,
        };
        self.refill();
        v
    }

    pub fn write(&mut self, port: u16, v: u8) {
        if port == 0x64 {
            self.status_cmd = true;
            self.controller_command(v);
            return;
        }
        self.status_cmd = false;
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::CommandByte => {
                self.command_byte = v;
                self.refill();
            }
            Pending::OutputPort => {
                self.output_port = v;
                if v & 1 == 0 {
                    self.reset_requested = true;
                }
            }
            Pending::KbdOutput => self.kbd_reply(&[v]),
            Pending::AuxOutput => self.aux_reply(&[v]),
            Pending::AuxWrite => match self.mouse.expect_arg.take() {
                Some(cmd) => self.mouse_arg(cmd, v),
                None => self.mouse_command(v),
            },
            Pending::KbdArg(cmd) => self.kbd_arg(cmd, v),
            Pending::None => self.kbd_command(v),
        }
    }

    fn controller_command(&mut self, v: u8) {
        match v {
            0x20 => self.controller_reply(self.command_byte),
            0x60 => self.pending = Pending::CommandByte,
            0xa7 => self.command_byte |= CB_AUX_OFF,
            0xa8 => {
                self.command_byte &= !CB_AUX_OFF;
                self.refill();
            }
            0xa9 | 0xab => self.controller_reply(0x00),
            0xaa => {
                self.command_byte |= CB_SYS;
                self.controller_reply(0x55);
            }
            0xad => self.command_byte |= CB_KBD_OFF,
            0xae => {
                self.command_byte &= !CB_KBD_OFF;
                self.refill();
            }
            0xc0 => self.controller_reply(0x80),
            0xd0 => self.controller_reply(self.output_port),
            0xd1 => self.pending = Pending::OutputPort,
            0xd2 => self.pending = Pending::KbdOutput,
            0xd3 => self.pending = Pending::AuxOutput,
            0xd4 => self.pending = Pending::AuxWrite,
            0xdd => self.output_port &= !2,
            0xdf => self.output_port |= 2,
            0xf0..=0xff => {
                if v & 1 == 0 {
                    self.reset_requested = true;
                }
            }
            _ => {}
        }
    }

    fn kbd_command(&mut self, v: u8) {
        self.command_byte &= !CB_KBD_OFF;
        match v {
            0xed | 0xf0 | 0xf3 => {
                self.pending = Pending::KbdArg(v);
                self.kbd_reply(&[0xfa]);
            }
            0xee => self.kbd_reply(&[0xee]),
            0xf2 => self.kbd_reply(&[0xfa, 0xab, 0x41]),
            0xf4 => {
                self.kbd_enabled = true;
                self.kbd_reply(&[0xfa]);
            }
            0xf5 => {
                self.kbd_enabled = false;
                self.kbd_queue.clear();
                self.kbd_reply(&[0xfa]);
            }
            0xf6 => {
                self.kbd_enabled = true;
                self.kbd_reply(&[0xfa]);
            }
            0xff => {
                self.kbd_queue.clear();
                self.kbd_enabled = true;
                self.kbd_reply(&[0xfa, 0xaa]);
            }
            _ => self.kbd_reply(&[0xfa]),
        }
    }

    fn kbd_arg(&mut self, cmd: u8, v: u8) {
        if cmd == 0xf0 && v == 0 {
            self.kbd_reply(&[0xfa, 0x41]);
        } else {
            self.kbd_reply(&[0xfa]);
        }
    }

    fn mouse_command(&mut self, v: u8) {
        let m = &mut self.mouse;
        match v {
            0xe6 => m.scaling21 = false,
            0xe7 => m.scaling21 = true,
            0xe8 | 0xf3 => {
                m.expect_arg = Some(v);
                self.aux_reply(&[0xfa]);
                return;
            }
            0xe9 => {
                let status = (m.reporting as u8) << 5 | (m.scaling21 as u8) << 4 | m.buttons;
                let (res, rate) = (m.resolution, m.sample_rate);
                self.aux_reply(&[0xfa, status, res, rate]);
                return;
            }
            0xf2 => {
                let id = m.id;
                self.aux_reply(&[0xfa, id]);
                return;
            }
            0xf4 => m.reporting = true,
            0xf5 => m.reporting = false,
            0xf6 => {
                *m = Mouse { sample_rate: 100, resolution: 2, id: m.id, ..Default::default() };
            }
            0xff => {
                *m = Mouse { sample_rate: 100, resolution: 2, ..Default::default() };
                self.aux_queue.clear();
                self.aux_reply(&[0xfa, 0xaa, 0x00]);
                return;
            }
            _ => {}
        }
        self.aux_reply(&[0xfa]);
    }

    fn mouse_arg(&mut self, cmd: u8, v: u8) {
        let m = &mut self.mouse;
        if cmd == 0xf3 {
            m.sample_rate = v;
            m.rate_history = [m.rate_history[1], m.rate_history[2], v];
            if m.rate_history == [200, 100, 80] {
                m.id = 3;
            }
        } else {
            m.resolution = v & 3;
        }
        self.aux_reply(&[0xfa]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_test_and_command_byte() {
        let mut k = I8042::default();
        k.write(0x64, 0xaa);
        assert_eq!(k.read(0x64) & STATUS_OBF, STATUS_OBF);
        assert_eq!(k.read(0x60), 0x55);
        k.write(0x64, 0x60);
        k.write(0x60, 0x65);
        k.write(0x64, 0x20);
        assert_eq!(k.read(0x60), 0x65);
    }

    #[test]
    fn keyboard_reset_and_keys_raise_irq1() {
        let mut k = I8042::default();
        k.write(0x60, 0xff);
        assert_eq!(k.irqs(), (true, false));
        assert_eq!(k.read(0x60), 0xfa);
        assert_eq!(k.read(0x60), 0xaa);
        assert_eq!(k.irqs(), (false, false));
        k.key(0xe048, true);
        assert_eq!(k.read(0x60), 0xe0);
        assert_eq!(k.read(0x60), 0x48);
    }

    #[test]
    fn mouse_intellimouse_detection_and_packets() {
        let mut k = I8042::default();
        k.write(0x64, 0x60);
        k.write(0x60, CB_KBD_INT | CB_AUX_INT | CB_TRANSLATE);
        for rate in [200, 100, 80] {
            k.write(0x64, 0xd4);
            k.write(0x60, 0xf3);
            assert_eq!(k.read(0x60), 0xfa);
            k.write(0x64, 0xd4);
            k.write(0x60, rate);
            assert_eq!(k.read(0x60), 0xfa);
        }
        k.write(0x64, 0xd4);
        k.write(0x60, 0xf2);
        assert_eq!(k.read(0x60), 0xfa);
        assert_eq!(k.read(0x60), 0x03);
        k.write(0x64, 0xd4);
        k.write(0x60, 0xf4);
        assert_eq!(k.read(0x60), 0xfa);
        k.mouse_motion(5, -3, 0, 1);
        assert_eq!(k.irqs(), (false, true));
        assert_eq!(read_n(&mut k), [0x29, 5, 0xfd, 0]);
        k.mouse_motion(0, 0, 1, 1);
        assert_eq!(read_n(&mut k), [0x09, 0, 0, 0xff]);
    }

    fn read_n<const N: usize>(k: &mut I8042) -> [u8; N] {
        std::array::from_fn(|_| k.read(0x60))
    }

    fn reporting_mouse() -> I8042 {
        let mut k = I8042::default();
        k.write(0x64, 0xd4);
        k.write(0x60, 0xf4);
        assert_eq!(k.read(0x60), 0xfa);
        k
    }

    fn packet_delta(p: [u8; 3]) -> (i32, i32) {
        let x = p[1] as i32 - if p[0] & 0x10 != 0 { 256 } else { 0 };
        let y = p[2] as i32 - if p[0] & 0x20 != 0 { 256 } else { 0 };
        (x, y)
    }

    #[test]
    fn large_motion_is_split_into_nine_bit_packets() {
        let mut k = reporting_mouse();
        k.mouse_motion(700, -300, 0, 0);
        let mut total = (0, 0);
        let mut packets = 0;
        while k.read(0x64) & STATUS_OBF != 0 {
            let (x, y) = packet_delta(read_n(&mut k));
            assert!((-256..=255).contains(&x) && (-256..=255).contains(&y));
            total = (total.0 + x, total.1 + y);
            packets += 1;
        }
        assert_eq!(total, (700, -300));
        assert_eq!(packets, 3);
    }

    #[test]
    fn motion_beyond_the_queue_waits_until_the_guest_reads() {
        let mut k = reporting_mouse();
        for _ in 0..40 {
            k.mouse_motion(200, 0, 0, 0);
        }
        let mut total = 0;
        while k.read(0x64) & STATUS_OBF != 0 {
            total += packet_delta(read_n(&mut k)).0;
        }
        assert!(total > MAX_PENDING && total < 40 * 200, "{total}");
    }

    #[test]
    fn a_button_change_without_motion_still_sends_a_packet() {
        let mut k = reporting_mouse();
        k.mouse_motion(0, 0, 0, 2);
        assert_eq!(read_n(&mut k), [0x0a, 0, 0]);
        k.mouse_motion(0, 0, 0, 2);
        assert_eq!(k.read(0x64) & STATUS_OBF, 0);
        k.mouse_motion(0, 0, 0, 0);
        assert_eq!(read_n(&mut k), [0x08, 0, 0]);
    }
}
