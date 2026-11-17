//! Network cards and the networks behind them.

mod eeprom;
pub mod rtl8139;
#[cfg(slirp)]
pub mod slirp;
pub mod user;

#[cfg(not(slirp))]
use libc as _;

pub use eeprom::Eeprom93c46;
pub use rtl8139::Rtl8139;

/// Guest physical memory as a bus-mastering card sees it.
pub trait DmaMemory {
    fn read_phys(&mut self, addr: u64, buf: &mut [u8]);
    fn write_phys(&mut self, addr: u64, buf: &[u8]);
}

/// The network a card is plugged into. Frames are Ethernet frames without
/// the frame check sequence.
pub trait Backend: Send {
    fn send(&mut self, frame: &[u8]);
    /// The next frame for the guest, if one is waiting.
    fn recv(&mut self) -> Option<Vec<u8>>;
}

/// No cable: frames from the guest go nowhere and none arrive.
pub struct Unplugged;

impl Backend for Unplugged {
    fn send(&mut self, _frame: &[u8]) {}
    fn recv(&mut self) -> Option<Vec<u8>> {
        None
    }
}

/// QEMU's default MAC address, 52:54:00:12:34:56.
pub const DEFAULT_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// Starts user-mode networking (see `slirp::Slirp::start`), or says why
/// this build cannot.
pub fn start_user_network(cfg: &user::UserNet, wake_guest: Box<dyn Fn() + Send + Sync>) -> Result<Box<dyn Backend>, String> {
    #[cfg(slirp)]
    return slirp::Slirp::start(cfg, wake_guest).map(|s| Box::new(s) as Box<dyn Backend>);
    #[cfg(not(slirp))]
    {
        let _ = (cfg, wake_guest);
        Err("this build has no libslirp; build it inside `nix develop`".into())
    }
}
