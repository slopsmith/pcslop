//! MC146818-compatible RTC and CMOS RAM (Motorola MC146818A datasheet).
//! Time comes from the host clock; alarms and update interrupts are not
//! implemented yet, but the periodic interrupt is.

use std::time::{SystemTime, UNIX_EPOCH};

pub const REG_A: usize = 0x0a;
pub const REG_B: usize = 0x0b;
pub const REG_C: usize = 0x0c;
pub const REG_D: usize = 0x0d;

const B_24H: u8 = 0x02;
const B_BINARY: u8 = 0x04;
const B_PIE: u8 = 0x40;
const B_SET: u8 = 0x80;
const C_PF: u8 = 0x40;
const C_IRQF: u8 = 0x80;

pub struct Rtc {
    pub ram: [u8; 128],
    index: u8,
    /// Seconds added to the host clock by guest writes to the time registers.
    offset: i64,
    next_periodic_ns: u64,
}

impl Default for Rtc {
    fn default() -> Self {
        let mut ram = [0; 128];
        ram[REG_A] = 0x26;
        ram[REG_B] = B_24H;
        ram[REG_D] = 0x80;
        Rtc { ram, index: 0, offset: 0, next_periodic_ns: 0 }
    }
}

/// Converts days since 1970-01-01 to (year, month, day); Howard Hinnant's
/// `civil_from_days` (<https://howardhinnant.github.io/date_algorithms.html>).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl Rtc {
    fn host_seconds() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
    }

    fn enc(&self, v: u32) -> u8 {
        if self.ram[REG_B] & B_BINARY != 0 { v as u8 } else { ((v / 10) << 4 | v % 10) as u8 }
    }

    fn dec(&self, v: u8) -> u32 {
        if self.ram[REG_B] & B_BINARY != 0 { v as u32 } else { (v >> 4) as u32 * 10 + (v & 0xf) as u32 }
    }

    fn refresh_time(&mut self) {
        if self.ram[REG_B] & B_SET != 0 {
            return;
        }
        let t = Self::host_seconds() + self.offset;
        let days = t.div_euclid(86_400);
        let secs = t.rem_euclid(86_400) as u32;
        let (y, mo, d) = civil_from_days(days);
        let hour = secs / 3600;
        self.ram[0x00] = self.enc(secs % 60);
        self.ram[0x02] = self.enc(secs / 60 % 60);
        self.ram[0x04] = if self.ram[REG_B] & B_24H != 0 {
            self.enc(hour)
        } else {
            let h12 = if hour % 12 == 0 { 12 } else { hour % 12 };
            self.enc(h12) | if hour >= 12 { 0x80 } else { 0 }
        };
        self.ram[0x06] = self.enc(((days + 4).rem_euclid(7) + 1) as u32);
        self.ram[0x07] = self.enc(d);
        self.ram[0x08] = self.enc(mo);
        self.ram[0x09] = self.enc((y % 100) as u32);
        self.ram[0x32] = self.enc((y / 100) as u32);
    }

    fn commit_time(&mut self) {
        let hour_raw = self.ram[0x04];
        let hour = if self.ram[REG_B] & B_24H != 0 {
            self.dec(hour_raw)
        } else {
            let h = self.dec(hour_raw & 0x7f) % 12;
            h + if hour_raw & 0x80 != 0 { 12 } else { 0 }
        };
        let year = self.dec(self.ram[0x32]) as i64 * 100 + self.dec(self.ram[0x09]) as i64;
        let days = days_from_civil(year, self.dec(self.ram[0x08]).max(1), self.dec(self.ram[0x07]).max(1));
        let t = days * 86_400 + hour as i64 * 3600 + self.dec(self.ram[0x02]) as i64 * 60 + self.dec(self.ram[0x00]) as i64;
        self.offset = t - Self::host_seconds();
    }

    fn periodic_ns(&self) -> Option<u64> {
        let rate = self.ram[REG_A] & 0x0f;
        if rate == 0 || self.ram[REG_B] & B_PIE == 0 {
            return None;
        }
        let rate = if rate < 3 { rate + 7 } else { rate };
        Some(1_000_000_000u64 * (1 << (rate - 1)) / 32_768)
    }

    /// Returns true when the periodic interrupt should be raised on IRQ 8.
    pub fn poll(&mut self, now_ns: u64) -> bool {
        let Some(period) = self.periodic_ns() else {
            self.next_periodic_ns = 0;
            return false;
        };
        if self.next_periodic_ns == 0 {
            self.next_periodic_ns = now_ns + period;
            return false;
        }
        if now_ns < self.next_periodic_ns {
            return false;
        }
        self.next_periodic_ns = now_ns + period;
        self.ram[REG_C] |= C_PF | C_IRQF;
        true
    }

    pub fn read(&mut self, port: u16) -> u8 {
        if port == 0x70 {
            return 0xff;
        }
        let i = self.index as usize;
        match i {
            0x00..=0x09 | 0x32 => {
                self.refresh_time();
                self.ram[i]
            }
            REG_A => {
                let uip = (std::time::SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_micros()))
                    >= 999_756;
                self.ram[REG_A] & 0x7f | if uip { 0x80 } else { 0 }
            }
            REG_C => {
                let v = self.ram[REG_C];
                self.ram[REG_C] = 0;
                v
            }
            _ => self.ram[i],
        }
    }

    pub fn write(&mut self, port: u16, v: u8) {
        if port == 0x70 {
            self.index = v & 0x7f;
            return;
        }
        let i = self.index as usize;
        match i {
            REG_A => self.ram[REG_A] = v & 0x7f,
            REG_B => {
                let was_set = self.ram[REG_B] & B_SET != 0;
                if v & B_SET != 0 && !was_set {
                    self.refresh_time();
                }
                self.ram[REG_B] = v;
                if v & B_SET == 0 && was_set {
                    self.commit_time();
                }
            }
            REG_C | REG_D => {}
            0x00..=0x09 | 0x32 => {
                self.refresh_time();
                self.ram[i] = v;
                if self.ram[REG_B] & B_SET == 0 {
                    self.commit_time();
                }
            }
            _ => self.ram[i] = v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_round_trip() {
        for days in [-1, 0, 11_016, 20_000, 20_773] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn guest_time_write_sticks() {
        let mut rtc = Rtc::default();
        rtc.write(0x70, 0x09);
        rtc.write(0x71, 0x03);
        rtc.write(0x70, 0x32);
        rtc.write(0x71, 0x20);
        rtc.write(0x70, 0x09);
        assert_eq!(rtc.read(0x71), 0x03);
    }

    #[test]
    fn writing_one_field_keeps_the_date() {
        let mut rtc = Rtc::default();
        rtc.write(0x70, 0x00);
        rtc.write(0x71, 0x00);
        rtc.write(0x70, 0x32);
        assert_eq!(rtc.read(0x71), 0x20);
        rtc.write(0x70, 0x08);
        assert_ne!(rtc.read(0x71), 0);
    }
}
