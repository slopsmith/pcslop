//! A control-method battery (`\_SB.BAT0`) and AC adapter (`\_SB.AC`) behind a
//! window of 32-bit I/O registers that the DSDT's `_STA`, `_BIF`, `_BST` and
//! `_PSR` read. Capacities are in mAh and voltages in mV. A change the guest
//! should hear about sets a bit in `EVENTS` and raises GPE0 bit
//! [`crate::pm::GPE_BATTERY`], whose `_E01` handler notifies the devices.

pub const BASE: u16 = 0x640;
pub const LEN: u16 = 0x30;

pub mod reg {
    /// [`super::status`] bits.
    pub const STATUS: u16 = 0x00;
    /// [`super::event`] bits, cleared by writing one.
    pub const EVENTS: u16 = 0x04;
    /// The `_BST` state, [`super::state`] bits.
    pub const STATE: u16 = 0x08;
    pub const RATE: u16 = 0x0c;
    pub const REMAINING: u16 = 0x10;
    pub const VOLTAGE: u16 = 0x14;
    pub const DESIGN_CAPACITY: u16 = 0x18;
    pub const FULL_CAPACITY: u16 = 0x1c;
    pub const DESIGN_VOLTAGE: u16 = 0x20;
    pub const WARNING: u16 = 0x24;
    pub const LOW: u16 = 0x28;
    pub const GRANULARITY: u16 = 0x2c;
}

pub mod status {
    pub const BATTERY_PRESENT: u32 = 1 << 0;
    pub const AC_ONLINE: u32 = 1 << 1;
}

pub mod event {
    /// Notify(AC, 0x80).
    pub const AC: u32 = 1 << 0;
    /// Notify(BAT0, 0x80): re-read `_BST`.
    pub const BATTERY_STATUS: u32 = 1 << 1;
    /// Notify(BAT0, 0x81): re-read `_STA` and `_BIF`.
    pub const BATTERY_INFO: u32 = 1 << 2;
}

pub mod state {
    pub const DISCHARGING: u32 = 1 << 0;
    pub const CHARGING: u32 = 1 << 1;
    pub const CRITICAL: u32 = 1 << 2;
}

const UNKNOWN: u32 = 0xffff_ffff;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Battery {
    pub design_mah: u32,
    pub full_mah: u32,
    pub remaining_mah: u32,
    pub design_mv: u32,
    pub present_mv: u32,
    /// Current into or out of the battery, if known.
    pub rate_ma: Option<u32>,
    pub charging: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Power {
    pub ac_online: bool,
    pub battery: Option<Battery>,
}

impl Default for Power {
    fn default() -> Self {
        Power { ac_online: true, battery: None }
    }
}

impl Battery {
    const FAKE_MAH: u32 = 5000;
    const FAKE_MV: u32 = 11_400;

    /// A made-up battery at `percent`, charging on AC until it is full.
    pub fn fake(percent: u8, ac_online: bool) -> Battery {
        let percent = percent.min(100) as u32;
        let charging = ac_online && percent < 100;
        Battery {
            design_mah: Self::FAKE_MAH,
            full_mah: Self::FAKE_MAH,
            remaining_mah: Self::FAKE_MAH * percent / 100,
            design_mv: Self::FAKE_MV,
            present_mv: Self::FAKE_MV,
            rate_ma: Some(if charging { 2000 } else if ac_online { 0 } else { 1500 }),
            charging,
        }
    }

    pub fn percent(&self) -> u32 {
        (self.remaining_mah as u64 * 100 / self.full_mah.max(1) as u64) as u32
    }

    fn low_mah(&self) -> u32 {
        self.full_mah / 50
    }

    fn state(&self, ac_online: bool) -> u32 {
        if self.charging {
            state::CHARGING
        } else if !ac_online {
            let critical = if self.remaining_mah < self.low_mah() { state::CRITICAL } else { 0 };
            state::DISCHARGING | critical
        } else {
            0
        }
    }
}

#[derive(Default)]
pub struct PowerDevice {
    power: Power,
    events: u32,
}

impl PowerDevice {
    pub fn new(power: Power) -> Self {
        PowerDevice { power, events: 0 }
    }

    pub fn power(&self) -> Power {
        self.power
    }

    /// Takes a new reading. Returns true when it sets an event the guest
    /// has not been told about yet, which should raise the GPE.
    pub fn set(&mut self, new: Power) -> bool {
        let old = std::mem::replace(&mut self.power, new);
        let mut ev = 0;
        if old.ac_online != new.ac_online {
            ev |= event::AC | event::BATTERY_STATUS;
        }
        match (old.battery, new.battery) {
            (Some(o), Some(n)) => {
                if (o.design_mah, o.full_mah, o.design_mv) != (n.design_mah, n.full_mah, n.design_mv) {
                    ev |= event::BATTERY_INFO;
                }
                if o.state(old.ac_online) != n.state(new.ac_online) || o.percent() != n.percent() {
                    ev |= event::BATTERY_STATUS;
                }
            }
            (None, None) => {}
            _ => ev |= event::BATTERY_INFO | event::BATTERY_STATUS,
        }
        let fresh = ev & !self.events != 0;
        self.events |= ev;
        fresh
    }

    /// Forgets events the guest never acknowledged, as a reset does.
    pub fn reset(&mut self) {
        self.events = 0;
    }

    fn reg(&self, offset: u16) -> u32 {
        let p = &self.power;
        let ac = if p.ac_online { status::AC_ONLINE } else { 0 };
        let Some(b) = p.battery else {
            return match offset {
                reg::STATUS => ac,
                reg::EVENTS => self.events,
                reg::STATE | reg::RATE | reg::REMAINING | reg::VOLTAGE => 0,
                _ => UNKNOWN,
            };
        };
        match offset {
            reg::STATUS => ac | status::BATTERY_PRESENT,
            reg::EVENTS => self.events,
            reg::STATE => b.state(p.ac_online),
            reg::RATE => b.rate_ma.unwrap_or(UNKNOWN),
            reg::REMAINING => b.remaining_mah,
            reg::VOLTAGE => b.present_mv,
            reg::DESIGN_CAPACITY => b.design_mah,
            reg::FULL_CAPACITY => b.full_mah,
            reg::DESIGN_VOLTAGE => b.design_mv,
            reg::WARNING => b.full_mah / 10,
            reg::LOW => b.low_mah(),
            reg::GRANULARITY => (b.full_mah / 100).max(1),
            _ => 0,
        }
    }

    pub fn read(&self, offset: u16, size: u8) -> u32 {
        let word = self.reg(offset & !3) as u64 | (self.reg((offset & !3) + 4) as u64) << 32;
        let v = (word >> (8 * (offset & 3))) as u32;
        if size >= 4 { v } else { v & ((1 << (8 * size)) - 1) }
    }

    pub fn write(&mut self, offset: u16, size: u8, value: u32) {
        if (reg::EVENTS..reg::EVENTS + 4).contains(&offset) {
            let mask = if size >= 4 { value } else { value & ((1 << (8 * size)) - 1) };
            self.events &= !(mask << (8 * (offset - reg::EVENTS)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on_battery(percent: u8) -> Power {
        Power { ac_online: false, battery: Some(Battery::fake(percent, false)) }
    }

    fn on_ac(percent: u8) -> Power {
        Power { ac_online: true, battery: Some(Battery::fake(percent, true)) }
    }

    #[test]
    fn registers_describe_a_discharging_battery() {
        let d = PowerDevice::new(on_battery(42));
        assert_eq!(d.read(reg::STATUS, 4), status::BATTERY_PRESENT);
        assert_eq!(d.read(reg::STATE, 4), state::DISCHARGING);
        assert_eq!(d.read(reg::REMAINING, 4), 2100);
        assert_eq!(d.read(reg::FULL_CAPACITY, 4), 5000);
        assert_eq!(d.read(reg::DESIGN_VOLTAGE, 4), 11_400);
        assert_eq!(d.read(reg::RATE, 4), 1500);
    }

    #[test]
    fn a_mac_without_a_battery_reports_ac_online_and_no_battery() {
        let d = PowerDevice::new(Power::default());
        assert_eq!(d.read(reg::STATUS, 4), status::AC_ONLINE);
        assert_eq!(d.read(reg::STATE, 4), 0);
        assert_eq!(d.read(reg::DESIGN_CAPACITY, 4), UNKNOWN);
    }

    #[test]
    fn charging_on_ac_until_full() {
        assert_eq!(PowerDevice::new(on_ac(80)).read(reg::STATE, 4), state::CHARGING);
        assert_eq!(PowerDevice::new(on_ac(100)).read(reg::STATE, 4), 0);
    }

    #[test]
    fn nearly_empty_discharging_battery_is_critical() {
        assert_eq!(PowerDevice::new(on_battery(1)).read(reg::STATE, 4), state::DISCHARGING | state::CRITICAL);
        assert_eq!(PowerDevice::new(on_battery(2)).read(reg::STATE, 4), state::DISCHARGING);
    }

    #[test]
    fn narrow_reads_return_the_addressed_bytes() {
        let d = PowerDevice::new(on_battery(42));
        assert_eq!(d.read(reg::REMAINING, 1), 2100 & 0xff);
        assert_eq!(d.read(reg::REMAINING + 1, 1), 2100 >> 8);
        assert_eq!(d.read(reg::REMAINING, 2), 2100);
    }

    #[test]
    fn unplugging_ac_notifies_both_devices() {
        let mut d = PowerDevice::new(on_ac(80));
        assert!(d.set(Power { ac_online: false, battery: Some(Battery::fake(80, false)) }));
        assert_eq!(d.read(reg::EVENTS, 4), event::AC | event::BATTERY_STATUS);
    }

    #[test]
    fn a_percent_step_notifies_the_battery_only() {
        let mut d = PowerDevice::new(on_battery(50));
        assert!(d.set(on_battery(49)));
        assert_eq!(d.read(reg::EVENTS, 4), event::BATTERY_STATUS);
    }

    #[test]
    fn voltage_and_rate_jitter_raise_nothing() {
        let mut d = PowerDevice::new(on_battery(50));
        let mut p = on_battery(50);
        let b = p.battery.as_mut().unwrap();
        b.present_mv += 37;
        b.rate_ma = Some(1234);
        b.remaining_mah += 10;
        assert!(!d.set(p));
        assert_eq!(d.read(reg::EVENTS, 4), 0);
    }

    #[test]
    fn removing_the_battery_asks_for_new_info() {
        let mut d = PowerDevice::new(on_ac(80));
        assert!(d.set(Power::default()));
        assert_eq!(d.read(reg::EVENTS, 4) & event::BATTERY_INFO, event::BATTERY_INFO);
        assert_eq!(d.read(reg::STATUS, 4), status::AC_ONLINE);
    }

    #[test]
    fn a_new_full_charge_capacity_asks_for_new_info() {
        let mut d = PowerDevice::new(on_battery(50));
        let mut p = on_battery(50);
        let b = p.battery.as_mut().unwrap();
        b.full_mah -= 100;
        b.remaining_mah = b.full_mah / 2;
        assert!(d.set(p));
        assert_eq!(d.read(reg::EVENTS, 4), event::BATTERY_INFO);
    }

    #[test]
    fn events_already_pending_do_not_raise_again() {
        let mut d = PowerDevice::new(on_battery(50));
        assert!(d.set(on_battery(49)));
        assert!(!d.set(on_battery(48)));
    }

    #[test]
    fn writing_one_clears_only_those_events() {
        let mut d = PowerDevice::new(on_ac(80));
        d.set(on_battery(80));
        d.write(reg::EVENTS, 4, event::AC);
        assert_eq!(d.read(reg::EVENTS, 4), event::BATTERY_STATUS);
        d.write(reg::STATE, 4, 0xffff_ffff);
        assert_eq!(d.read(reg::EVENTS, 4), event::BATTERY_STATUS);
    }

    #[test]
    fn an_acknowledged_change_raises_again_next_time() {
        let mut d = PowerDevice::new(on_battery(50));
        d.set(on_battery(49));
        d.write(reg::EVENTS, 4, d.read(reg::EVENTS, 4));
        assert!(d.set(on_battery(48)));
    }
}
