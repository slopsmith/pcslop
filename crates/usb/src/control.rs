//! Endpoint 0 control transfers: the SETUP, data and status stages.
//!
//! The stage tracking follows `do_token_setup`, `do_token_in` and
//! `do_token_out` in QEMU's hw/usb/core.c (Copyright (c) 2005 Fabrice Bellard,
//! (c) 2008 Max Krasnyansky, MIT license).

use crate::{Pid, Reply};

/// Largest data stage we accept, as in QEMU.
const MAX_DATA: usize = 4096;

pub const DIR_IN: u8 = 0x80;

#[derive(Clone, Copy, Default, Debug)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    fn parse(b: &[u8]) -> Setup {
        Setup {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        }
    }

    fn device_to_host(&self) -> bool {
        self.request_type & DIR_IN != 0
    }
}

pub trait ControlHandler {
    /// Handles a request. For device-to-host requests, writes the answer into
    /// `data` (sized to wLength) and returns its length. For host-to-device
    /// requests, `data` holds what the host sent. `None` stalls the request.
    fn control(&mut self, setup: &Setup, data: &mut [u8]) -> Option<usize>;
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Stage {
    #[default]
    Idle,
    Data,
    Status,
}

#[derive(Default)]
pub struct ControlPipe {
    setup: Setup,
    stage: Stage,
    data: Vec<u8>,
    len: usize,
    pos: usize,
}

impl ControlPipe {
    pub fn reset(&mut self) {
        self.stage = Stage::Idle;
    }

    pub fn transact(&mut self, h: &mut dyn ControlHandler, pid: Pid, buf: &mut [u8]) -> Reply {
        match pid {
            Pid::Setup => self.setup_stage(h, buf),
            Pid::In => self.in_token(h, buf),
            Pid::Out => self.out_token(buf),
        }
    }

    fn setup_stage(&mut self, h: &mut dyn ControlHandler, buf: &[u8]) -> Reply {
        self.stage = Stage::Idle;
        if buf.len() != 8 {
            return Reply::Stall;
        }
        let s = Setup::parse(buf);
        self.setup = s;
        self.len = s.length as usize;
        self.pos = 0;
        if self.len > MAX_DATA {
            return Reply::Stall;
        }
        self.data.clear();
        self.data.resize(self.len, 0);
        if s.device_to_host() {
            match h.control(&s, &mut self.data) {
                Some(n) => self.len = n.min(self.len),
                None => return Reply::Stall,
            }
            self.stage = Stage::Data;
        } else {
            self.stage = if self.len == 0 { Stage::Status } else { Stage::Data };
        }
        Reply::Ack(8)
    }

    fn in_token(&mut self, h: &mut dyn ControlHandler, buf: &mut [u8]) -> Reply {
        let to_host = self.setup.device_to_host();
        match self.stage {
            // Status stage of a host-to-device request: this is when it takes effect.
            Stage::Status if !to_host => {
                self.stage = Stage::Idle;
                match h.control(&self.setup, &mut self.data) {
                    Some(_) => Reply::Ack(0),
                    None => Reply::Stall,
                }
            }
            Stage::Status => Reply::Ack(0),
            Stage::Data if to_host => {
                let n = (self.len - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                if self.pos >= self.len {
                    self.stage = Stage::Status;
                }
                Reply::Ack(n)
            }
            _ => {
                self.stage = Stage::Idle;
                Reply::Stall
            }
        }
    }

    fn out_token(&mut self, buf: &[u8]) -> Reply {
        let to_host = self.setup.device_to_host();
        match self.stage {
            Stage::Status if to_host => {
                self.stage = Stage::Idle;
                Reply::Ack(0)
            }
            Stage::Status => Reply::Ack(0),
            Stage::Data if !to_host => {
                let n = (self.len - self.pos).min(buf.len());
                self.data[self.pos..self.pos + n].copy_from_slice(&buf[..n]);
                self.pos += n;
                if self.pos >= self.len {
                    self.stage = Stage::Status;
                }
                Reply::Ack(n)
            }
            // The host may end an IN data stage early with the status packet.
            Stage::Data if buf.is_empty() => {
                self.stage = Stage::Idle;
                Reply::Ack(0)
            }
            _ => {
                self.stage = Stage::Idle;
                Reply::Stall
            }
        }
    }
}
