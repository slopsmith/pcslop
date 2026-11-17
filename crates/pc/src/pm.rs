//! The PIIX4 power management function (82371AB function 3, datasheet
//! chapter 7) as an ACPI fixed-hardware block: PM1 event and control, the
//! 24-bit PM timer and GPE0, with the SCI on IRQ 9. Its SMBus host controller
//! answers every transaction with "no device". The register behaviour follows
//! QEMU's `hw/acpi/piix4.c` and `hw/acpi/core.c` (Fabrice Bellard, Gleb Natapov
//! and the QEMU authors).

use crate::pci::ConfigSpace;

pub const PCI_DEVICE: u16 = 0x7113;
pub const SCI_IRQ: u8 = 9;
/// Bytes of PM I/O space decoded at PMBA.
pub const IO_LEN: u16 = 64;
pub const SMBUS_LEN: u16 = 16;

pub const PMBA: usize = 0x40;
pub const DEVACTB: usize = 0x58;
pub const PMREGMISC: usize = 0x80;
pub const SMBBA: usize = 0x90;
pub const SMBHSTCFG: usize = 0xd2;

/// Offsets in the PM I/O space.
pub mod reg {
    pub const PM1_STS: u16 = 0x00;
    pub const PM1_EN: u16 = 0x02;
    pub const PM1_CNT: u16 = 0x04;
    pub const PM_TMR: u16 = 0x08;
    pub const GPE0_STS: u16 = 0x0c;
    pub const GPE0_EN: u16 = 0x0e;
    pub const GPE0_LEN: u8 = 4;
}

pub mod pm1 {
    pub const TMR: u16 = 1 << 0;
    pub const GBL: u16 = 1 << 5;
    pub const PWRBTN: u16 = 1 << 8;
    pub const RTC: u16 = 1 << 10;
    pub const WAK: u16 = 1 << 15;
    pub const SCI_EN: u16 = 1 << 0;
    pub const SLP_TYP_SHIFT: u16 = 10;
    pub const SLP_TYP: u16 = 7 << SLP_TYP_SHIFT;
    pub const SLP_EN: u16 = 1 << 13;
}

/// SLP_TYP value of the soft-off (S5) state.
pub const SLP_TYP_SOFF: u16 = 0;
/// Values written to the APM control port (0xB2) to switch SCI_EN.
pub const ACPI_ENABLE: u8 = 0xf1;
pub const ACPI_DISABLE: u8 = 0xf0;
pub const PORT_APM_CNT: u16 = 0xb2;
pub const PORT_APM_STS: u16 = 0xb3;
/// GPE0 bit raised by the power button, handled by `\_GPE._E00` in the DSDT.
pub const GPE_POWER_BUTTON: u8 = 0;

const PM_TIMER_HZ: u128 = 3_579_545;
const SMB_HST_STS: u16 = 0;
const SMB_HST_CNT: u16 = 2;
const SMB_START: u8 = 0x40;
const SMB_DEV_ERR: u8 = 0x04;

pub fn config_space() -> ConfigSpace {
    let mut c = ConfigSpace::new(0x8086, PCI_DEVICE, 0x06_80_00, 0x03, 0x00);
    c.data[0x06] = 0x80;
    c.data[0x07] = 0x02;
    c.data[crate::pci::INTERRUPT_PIN] = 1;
    c.data[PMBA] = 1;
    c.writable[PMBA..PMBA + 4].copy_from_slice(&[0xc0, 0xff, 0, 0]);
    c.data[SMBBA] = 1;
    c.writable[SMBBA..SMBBA + 4].copy_from_slice(&[0xf0, 0xff, 0, 0]);
    // DEVACTB.APMC_EN set tells SeaBIOS that SMM is already set up, as QEMU
    // does when it has no SMM. We cannot take SMIs.
    c.data[DEVACTB + 3] = 0x02;
    c
}

/// Base of the PM I/O space when the guest has enabled it.
pub fn io_base(c: &ConfigSpace) -> Option<u16> {
    let base = c.read(PMBA, 2) as u16 & 0xffc0;
    (c.data[PMREGMISC] & 1 != 0 && base != 0).then_some(base)
}

pub fn smbus_base(c: &ConfigSpace) -> Option<u16> {
    let base = c.read(SMBBA, 2) as u16 & 0xfff0;
    (c.data[SMBHSTCFG] & 1 != 0 && base != 0).then_some(base)
}

#[derive(Default)]
pub struct Pm {
    sts: u16,
    en: u16,
    cnt: u16,
    gpe_sts: [u8; 2],
    gpe_en: [u8; 2],
    /// PM timer bit 23 when TMR_STS was last cleared.
    tmr_epoch: u64,
    apm_sts: u8,
    smb: [u8; SMBUS_LEN as usize],
    pub power_off_requested: bool,
}

impl Pm {
    pub fn reset(&mut self) {
        *self = Pm::default();
    }

    fn ticks(now_ns: u64) -> u64 {
        (now_ns as u128 * PM_TIMER_HZ / 1_000_000_000) as u64
    }

    fn update_timer_status(&mut self, now_ns: u64) {
        if Self::ticks(now_ns) >> 23 != self.tmr_epoch {
            self.sts |= pm1::TMR;
        }
    }

    /// Level of the SCI line.
    pub fn sci(&mut self, now_ns: u64) -> bool {
        self.update_timer_status(now_ns);
        let pm1 = self.sts & self.en & (pm1::TMR | pm1::GBL | pm1::PWRBTN | pm1::RTC) != 0;
        let gpe = (self.gpe_sts[0] & self.gpe_en[0]) | (self.gpe_sts[1] & self.gpe_en[1]) != 0;
        pm1 || gpe
    }

    pub fn raise_gpe(&mut self, bit: u8) {
        self.gpe_sts[bit as usize / 8] |= 1 << (bit % 8);
    }

    pub fn read(&mut self, offset: u16, size: u8, now_ns: u64) -> u32 {
        (0..size as u16).fold(0, |v, i| v | (self.read8(offset + i, now_ns) as u32) << (8 * i))
    }

    pub fn write(&mut self, offset: u16, size: u8, value: u32, now_ns: u64) {
        for i in 0..size as u16 {
            self.write8(offset + i, (value >> (8 * i)) as u8, now_ns);
        }
    }

    fn read8(&mut self, offset: u16, now_ns: u64) -> u8 {
        let byte = |v: u16, hi: bool| if hi { (v >> 8) as u8 } else { v as u8 };
        match offset {
            0..=1 => {
                self.update_timer_status(now_ns);
                byte(self.sts, offset == 1)
            }
            2..=3 => byte(self.en, offset == 3),
            4..=5 => byte(self.cnt, offset == 5),
            8..=11 => ((Self::ticks(now_ns) & 0xff_ffff) >> (8 * (offset - 8))) as u8,
            0x0c..=0x0d => self.gpe_sts[(offset - 0x0c) as usize],
            0x0e..=0x0f => self.gpe_en[(offset - 0x0e) as usize],
            _ => 0,
        }
    }

    fn write8(&mut self, offset: u16, v: u8, now_ns: u64) {
        let set_byte = |r: &mut u16, hi: bool| *r = if hi { (*r & 0xff) | (v as u16) << 8 } else { (*r & 0xff00) | v as u16 };
        match offset {
            0..=1 => {
                self.update_timer_status(now_ns);
                let clear = (v as u16) << (8 * offset);
                if clear & self.sts & pm1::TMR != 0 {
                    self.tmr_epoch = Self::ticks(now_ns) >> 23;
                }
                self.sts &= !clear;
            }
            2..=3 => set_byte(&mut self.en, offset == 3),
            4..=5 => {
                set_byte(&mut self.cnt, offset == 5);
                if self.cnt & pm1::SLP_EN != 0 {
                    self.cnt &= !pm1::SLP_EN;
                    if (self.cnt & pm1::SLP_TYP) >> pm1::SLP_TYP_SHIFT == SLP_TYP_SOFF {
                        self.power_off_requested = true;
                    }
                }
            }
            0x0c..=0x0d => self.gpe_sts[(offset - 0x0c) as usize] &= !v,
            0x0e..=0x0f => self.gpe_en[(offset - 0x0e) as usize] = v,
            _ => {}
        }
    }

    /// The APM control and status ports. Without SMM, the control port only
    /// switches SCI_EN, as on QEMU.
    pub fn apm_write(&mut self, port: u16, v: u8) {
        match (port, v) {
            (PORT_APM_CNT, ACPI_ENABLE) => self.cnt |= pm1::SCI_EN,
            (PORT_APM_CNT, ACPI_DISABLE) => self.cnt &= !pm1::SCI_EN,
            (PORT_APM_STS, _) => self.apm_sts = v,
            _ => {}
        }
    }

    pub fn apm_read(&self, port: u16) -> u8 {
        if port == PORT_APM_STS { self.apm_sts } else { 0 }
    }

    pub fn smbus_read(&self, offset: u16) -> u8 {
        self.smb.get(offset as usize).copied().unwrap_or(0)
    }

    pub fn smbus_write(&mut self, offset: u16, v: u8) {
        match offset {
            SMB_HST_STS => self.smb[0] &= !v,
            SMB_HST_CNT => {
                self.smb[2] = v & !SMB_START;
                if v & SMB_START != 0 {
                    self.smb[0] |= SMB_DEV_ERR;
                }
            }
            o if o < SMBUS_LEN => self.smb[o as usize] = v,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: u64 = 1_000_000_000;

    #[test]
    fn pm_timer_counts_at_3_58_mhz_and_wraps_at_24_bits() {
        let mut pm = Pm::default();
        assert_eq!(pm.read(reg::PM_TMR, 4, SECOND), 3_579_545);
        let wrap_ns = (1u64 << 24) * SECOND / 3_579_545 + 1;
        assert!(pm.read(reg::PM_TMR, 4, wrap_ns) < 10);
    }

    #[test]
    fn timer_status_sets_when_bit_23_flips_and_clears_on_write() {
        let mut pm = Pm::default();
        let half_wrap = (1u64 << 23) * SECOND / 3_579_545 + 1;
        assert_eq!(pm.read(reg::PM1_STS, 2, 1000) as u16 & pm1::TMR, 0);
        assert_ne!(pm.read(reg::PM1_STS, 2, half_wrap) as u16 & pm1::TMR, 0);
        pm.write(reg::PM1_STS, 2, pm1::TMR as u32, half_wrap);
        assert_eq!(pm.read(reg::PM1_STS, 2, half_wrap + 1000) as u16 & pm1::TMR, 0);
    }

    #[test]
    fn timer_status_raises_sci_only_when_enabled() {
        let mut pm = Pm::default();
        let half_wrap = (1u64 << 23) * SECOND / 3_579_545 + 1;
        assert!(!pm.sci(half_wrap));
        pm.write(reg::PM1_EN, 2, pm1::TMR as u32, half_wrap);
        assert!(pm.sci(half_wrap));
    }

    #[test]
    fn writing_sleep_enable_with_type_zero_powers_off() {
        let mut pm = Pm::default();
        pm.write(reg::PM1_CNT, 2, (SLP_TYP_SOFF << pm1::SLP_TYP_SHIFT | pm1::SLP_EN) as u32, 0);
        assert!(pm.power_off_requested);
        assert_eq!(pm.read(reg::PM1_CNT, 2, 0) as u16 & pm1::SLP_EN, 0);
    }

    #[test]
    fn other_sleep_types_do_not_power_off() {
        let mut pm = Pm::default();
        pm.write(reg::PM1_CNT, 2, (1 << pm1::SLP_TYP_SHIFT | pm1::SLP_EN) as u32, 0);
        assert!(!pm.power_off_requested);
    }

    #[test]
    fn byte_writes_to_the_control_register_power_off() {
        let mut pm = Pm::default();
        pm.write(reg::PM1_CNT, 1, pm1::SCI_EN as u32, 0);
        pm.write(reg::PM1_CNT + 1, 1, (pm1::SLP_EN >> 8) as u32, 0);
        assert!(pm.power_off_requested);
        assert_eq!(pm.read(reg::PM1_CNT, 2, 0) as u16, pm1::SCI_EN);
    }

    #[test]
    fn apm_port_switches_sci_enable() {
        let mut pm = Pm::default();
        pm.apm_write(PORT_APM_CNT, ACPI_ENABLE);
        assert_eq!(pm.read(reg::PM1_CNT, 2, 0) as u16 & pm1::SCI_EN, pm1::SCI_EN);
        pm.apm_write(PORT_APM_CNT, ACPI_DISABLE);
        assert_eq!(pm.read(reg::PM1_CNT, 2, 0) as u16 & pm1::SCI_EN, 0);
    }

    #[test]
    fn gpe_raises_sci_when_enabled_and_clears_by_writing_one() {
        let mut pm = Pm::default();
        pm.raise_gpe(GPE_POWER_BUTTON);
        assert!(!pm.sci(0));
        pm.write(reg::GPE0_EN, 1, 1, 0);
        assert!(pm.sci(0));
        assert_eq!(pm.read(reg::GPE0_STS, 1, 0), 1);
        pm.write(reg::GPE0_STS, 1, 1, 0);
        assert!(!pm.sci(0));
    }

    #[test]
    fn pm1_status_bits_clear_only_where_one_is_written() {
        let mut pm = Pm { sts: pm1::PWRBTN | pm1::RTC, ..Default::default() };
        pm.write(reg::PM1_STS, 2, pm1::PWRBTN as u32, 0);
        assert_eq!(pm.read(reg::PM1_STS, 2, 0) as u16, pm1::RTC);
    }

    #[test]
    fn config_space_decodes_pm_io_after_seabios_setup() {
        let mut c = config_space();
        assert_eq!(c.read(0, 4), 0x7113_8086);
        assert_eq!(io_base(&c), None);
        c.write(PMBA, 4, 0xb000 | 1);
        c.write(PMREGMISC, 1, 1);
        c.write(SMBBA, 4, 0xb100 | 1);
        c.write(SMBHSTCFG, 1, 9);
        assert_eq!(io_base(&c), Some(0xb000));
        assert_eq!(smbus_base(&c), Some(0xb100));
        c.write(PMBA, 4, 0xffff_ffff);
        assert_eq!(c.read(PMBA, 4), 0xffc1);
    }

    #[test]
    fn devactb_reports_smm_already_set_up() {
        let c = config_space();
        assert_ne!(c.read(DEVACTB, 4) & 1 << 25, 0);
    }

    #[test]
    fn smbus_transactions_find_no_device() {
        let mut pm = Pm::default();
        pm.smbus_write(SMB_HST_CNT, 0x48);
        assert_eq!(pm.smbus_read(SMB_HST_STS), SMB_DEV_ERR);
        pm.smbus_write(SMB_HST_STS, 0xff);
        assert_eq!(pm.smbus_read(SMB_HST_STS), 0);
    }
}
