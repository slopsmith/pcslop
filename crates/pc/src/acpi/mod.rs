//! ACPI 1.0b tables for the board: RSDP, RSDT, FADT, FACS and DSDT. SeaBIOS
//! installs them from three fw_cfg files, as QEMU serves them in
//! `hw/i386/acpi-build.c`: `etc/acpi/tables` holds every table but the RSDP,
//! `etc/acpi/rsdp` holds the RSDP, and `etc/table-loader` links them.

pub mod aml;
mod dsdt;
pub mod loader;

use crate::pm;
use loader::{Loader, Zone};

pub const TABLES_FILE: &str = "etc/acpi/tables";
pub const RSDP_FILE: &str = "etc/acpi/rsdp";
pub const LOADER_FILE: &str = "etc/table-loader";

/// Where SeaBIOS places the PIIX4 PM I/O space when fw_cfg has a table
/// loader (`qemu_cfg_init` in `src/fw/paravirt.c`).
pub const PM_BASE: u16 = 0x600;

const OEM_ID: &[u8; 6] = b"86SLOP";
const OEM_TABLE_ID: &[u8; 8] = b"SLOPSTN ";
const CREATOR_ID: &[u8; 4] = b"SLOP";
const HEADER_LEN: usize = 36;
const CHECKSUM_OFFSET: usize = 9;
const FACS_LEN: usize = 64;
const FADT_LEN: usize = 116;
const FADT_FIRMWARE_CTRL: usize = 36;
const FADT_DSDT: usize = 40;
const RTC_CENTURY: u8 = 0x32;
/// The ACPI processor ID of the CPU, shared by the DSDT and the MADT.
const CPU_ID: u8 = 0;

pub struct Files {
    pub tables: Vec<u8>,
    pub rsdp: Vec<u8>,
    pub loader: Vec<u8>,
}

struct Builder {
    blob: Vec<u8>,
    loader: Loader,
    rsdt_entries: Vec<u32>,
}

impl Builder {
    fn new() -> Self {
        let mut loader = Loader::default();
        loader.allocate(RSDP_FILE, 16, Zone::FSeg);
        loader.allocate(TABLES_FILE, 64, Zone::High);
        Builder { blob: Vec::new(), loader, rsdt_entries: Vec::new() }
    }

    fn push(&mut self, table: Vec<u8>, align: usize) -> u32 {
        self.blob.resize(self.blob.len().next_multiple_of(align), 0);
        let offset = self.blob.len() as u32;
        self.blob.extend(table);
        offset
    }

    fn link(&mut self, field: u32, target: u32) {
        let f = field as usize;
        self.blob[f..f + 4].copy_from_slice(&target.to_le_bytes());
        self.loader.add_pointer(TABLES_FILE, field, 4, TABLES_FILE);
    }

    fn checksum(&mut self, table: u32) {
        let len = u32::from_le_bytes(self.blob[table as usize + 4..table as usize + 8].try_into().unwrap());
        self.loader.add_checksum(TABLES_FILE, table + CHECKSUM_OFFSET as u32, table, len);
    }

    /// Appends a table that the RSDT lists. `pointers` are (field offset in
    /// the table, blob offset of the target).
    fn add_table(&mut self, table: Vec<u8>, pointers: &[(usize, u32)]) -> u32 {
        let offset = self.push(table, 4);
        for &(field, target) in pointers {
            self.link(offset + field as u32, target);
        }
        self.checksum(offset);
        self.rsdt_entries.push(offset);
        offset
    }

    fn finish(mut self) -> Files {
        let entries = std::mem::take(&mut self.rsdt_entries);
        let rsdt = self.push(header(b"RSDT", 1, HEADER_LEN + 4 * entries.len()), 4);
        for (i, &target) in entries.iter().enumerate() {
            self.link(rsdt + (HEADER_LEN + 4 * i) as u32, target);
        }
        self.checksum(rsdt);

        let mut rsdp = b"RSD PTR ".to_vec();
        rsdp.push(0);
        rsdp.extend(OEM_ID);
        rsdp.push(0);
        rsdp.extend(rsdt.to_le_bytes());
        self.loader.add_pointer(RSDP_FILE, 16, 4, TABLES_FILE);
        self.loader.add_checksum(RSDP_FILE, 8, 0, rsdp.len() as u32);
        Files { tables: self.blob, rsdp, loader: self.loader.into_bytes() }
    }
}

fn header(signature: &[u8; 4], revision: u8, len: usize) -> Vec<u8> {
    let mut h = signature.to_vec();
    h.extend((len as u32).to_le_bytes());
    h.push(revision);
    h.push(0);
    h.extend(OEM_ID);
    h.extend(OEM_TABLE_ID);
    h.extend(1u32.to_le_bytes());
    h.extend(CREATOR_ID);
    h.extend(1u32.to_le_bytes());
    h.resize(len, 0);
    h
}

fn facs() -> Vec<u8> {
    let mut t = b"FACS".to_vec();
    t.extend((FACS_LEN as u32).to_le_bytes());
    t.resize(FACS_LEN, 0);
    t
}

fn dsdt(ram_bytes: u32) -> Vec<u8> {
    let body = dsdt::body(ram_bytes);
    let mut t = header(b"DSDT", 1, HEADER_LEN + body.len());
    t[HEADER_LEN..].copy_from_slice(&body);
    t
}

mod fadt_flag {
    pub const WBINVD: u32 = 1 << 0;
    pub const PROC_C1: u32 = 1 << 2;
    pub const PWR_BUTTON: u32 = 1 << 4;
    pub const SLP_BUTTON: u32 = 1 << 5;
}

fn fadt() -> Vec<u8> {
    let mut t = header(b"FACP", 1, FADT_LEN);
    let put16 = |t: &mut Vec<u8>, o: usize, v: u16| t[o..o + 2].copy_from_slice(&v.to_le_bytes());
    let put32 = |t: &mut Vec<u8>, o: usize, v: u32| t[o..o + 4].copy_from_slice(&v.to_le_bytes());
    let base = PM_BASE as u32;
    put16(&mut t, 46, pm::SCI_IRQ as u16);
    put32(&mut t, 48, pm::PORT_APM_CNT as u32);
    t[52] = pm::ACPI_ENABLE;
    t[53] = pm::ACPI_DISABLE;
    put32(&mut t, 56, base + pm::reg::PM1_STS as u32);
    put32(&mut t, 64, base + pm::reg::PM1_CNT as u32);
    put32(&mut t, 76, base + pm::reg::PM_TMR as u32);
    put32(&mut t, 80, base + pm::reg::GPE0_STS as u32);
    t[88] = 4;
    t[89] = 2;
    t[91] = 4;
    t[92] = pm::reg::GPE0_LEN;
    put16(&mut t, 96, 0x0fff);
    put16(&mut t, 98, 0x0fff);
    t[108] = RTC_CENTURY;
    // The power button is the DSDT's PWRB device, not the fixed-feature one.
    put32(&mut t, 112, fadt_flag::WBINVD | fadt_flag::PROC_C1 | fadt_flag::PWR_BUTTON | fadt_flag::SLP_BUTTON);
    t
}

mod madt_flag {
    pub const PCAT_COMPAT: u32 = 1;
    pub const ENABLED: u32 = 1;
    pub const LEVEL_ACTIVE_HIGH: u16 = 0x000d;
}

// Overrides as in QEMU's `build_madt` (hw/i386/acpi-common.c, the QEMU
// authors): IRQ 0 on I/O APIC input 2, PCI and SCI IRQs level-triggered.
fn madt() -> Vec<u8> {
    let mut t = header(b"APIC", 1, HEADER_LEN);
    t.extend(crate::apic::DEFAULT_BASE.to_le_bytes());
    t.extend(madt_flag::PCAT_COMPAT.to_le_bytes());
    t.extend([0, 8, CPU_ID, 0]);
    t.extend(madt_flag::ENABLED.to_le_bytes());
    t.extend([1, 12, 0, 0]);
    t.extend(crate::ioapic::BASE.to_le_bytes());
    t.extend(0u32.to_le_bytes());
    let mut source_override = |irq: u8, gsi: u32, flags: u16| {
        t.extend([2, 10, 0, irq]);
        t.extend(gsi.to_le_bytes());
        t.extend(flags.to_le_bytes());
    };
    source_override(0, 2, 0);
    for irq in [5, 9, 10, 11] {
        source_override(irq, irq as u32, madt_flag::LEVEL_ACTIVE_HIGH);
    }
    t.extend([4, 6, 0xff, 0, 0, 1]);
    let len = t.len() as u32;
    t[4..8].copy_from_slice(&len.to_le_bytes());
    t
}

/// Builds the ACPI fw_cfg files for a board with `ram_bytes` of RAM below
/// 4 GiB, with a MADT when it has a local APIC and an I/O APIC.
pub fn build(ram_bytes: u32, apic: bool) -> Files {
    let mut b = Builder::new();
    let facs = b.push(facs(), 64);
    let dsdt = b.push(dsdt(ram_bytes), 4);
    b.checksum(dsdt);
    b.add_table(fadt(), &[(FADT_FIRMWARE_CTRL, facs), (FADT_DSDT, dsdt)]);
    if apic {
        b.add_table(madt(), &[]);
    }
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const TABLES_AT: u32 = 0x1fff_0000;
    const RSDP_AT: u32 = 0xf_5a40;

    type Memory = BTreeMap<String, (u32, Vec<u8>)>;

    fn load() -> Memory {
        load_with(false)
    }

    fn load_with(apic: bool) -> Memory {
        let f = build(512 << 20, apic);
        let files = [(TABLES_FILE, f.tables), (RSDP_FILE, f.rsdp)];
        loader::execute(&f.loader, &files, |name, align, zone| {
            let addr = if name == TABLES_FILE { TABLES_AT } else { RSDP_AT };
            assert_eq!(addr % align, 0);
            assert_eq!(zone == Zone::FSeg, name == RSDP_FILE);
            addr
        })
    }

    fn sum(b: &[u8]) -> u8 {
        b.iter().fold(0u8, |a, &x| a.wrapping_add(x))
    }

    fn u32_at(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }

    fn table(mem: &Memory, addr: u32) -> &[u8] {
        let (base, data) = &mem[TABLES_FILE];
        let off = (addr - base) as usize;
        let len = u32_at(data, off + 4) as usize;
        &data[off..off + len]
    }

    fn rsdt_tables(mem: &Memory) -> Vec<&[u8]> {
        let rsdt = table(mem, u32_at(&mem[RSDP_FILE].1, 16));
        (0..(rsdt.len() - HEADER_LEN) / 4).map(|i| table(mem, u32_at(rsdt, HEADER_LEN + 4 * i))).collect()
    }

    #[test]
    fn rsdp_is_valid_and_points_at_the_rsdt() {
        let mem = load();
        let rsdp = &mem[RSDP_FILE].1;
        assert_eq!(&rsdp[..8], b"RSD PTR ");
        assert_eq!(rsdp.len(), 20);
        assert_eq!(sum(rsdp), 0);
        let rsdt = table(&mem, u32_at(rsdp, 16));
        assert_eq!(&rsdt[..4], b"RSDT");
        assert_eq!(sum(rsdt), 0);
    }

    #[test]
    fn every_table_checksums_to_zero() {
        let mem = load();
        for t in rsdt_tables(&mem) {
            assert_eq!(sum(t), 0, "{} would be rejected by the OS", String::from_utf8_lossy(&t[..4]));
            if &t[..4] == b"FACP" {
                assert_eq!(sum(table(&mem, u32_at(t, FADT_DSDT))), 0, "the DSDT would be rejected by the OS");
            }
        }
    }

    #[test]
    fn rsdt_lists_the_fadt_which_points_at_facs_and_dsdt() {
        let mem = load();
        let tables = rsdt_tables(&mem);
        let sigs: Vec<&[u8]> = tables.iter().map(|t| &t[..4]).collect();
        assert_eq!(sigs, [b"FACP"]);
        let facs = u32_at(tables[0], FADT_FIRMWARE_CTRL);
        assert_eq!(facs % 64, 0);
        assert_eq!(&table(&mem, facs)[..4], b"FACS");
        assert_eq!(&table(&mem, u32_at(tables[0], FADT_DSDT))[..4], b"DSDT");
    }

    #[test]
    fn madt_describes_the_apics_for_an_apic_mode_os() {
        let mem = load_with(true);
        let tables = rsdt_tables(&mem);
        let madt = *tables.iter().find(|t| &t[..4] == b"APIC").expect("an APIC-mode OS needs a MADT");
        assert_eq!(sum(madt), 0);
        assert_eq!(u32_at(madt, 36), 0xfee0_0000);
        let mut entries = Vec::new();
        let mut i = 44;
        while i < madt.len() {
            entries.push(&madt[i..i + madt[i + 1] as usize]);
            i += madt[i + 1] as usize;
        }
        assert_eq!(i, madt.len());
        assert_eq!(entries[0], [0, 8, 0, 0, 1, 0, 0, 0]);
        assert_eq!(entries[1], [1, 12, 0, 0, 0, 0, 0xc0, 0xfe, 0, 0, 0, 0]);
        assert!(entries.contains(&&[2, 10, 0, 0, 2, 0, 0, 0, 0, 0][..]), "IRQ 0 must be on I/O APIC input 2");
        assert!(entries.contains(&&[2, 10, 0, 9, 9, 0, 0, 0, 0x0d, 0][..]), "the SCI must be level-triggered");
        assert_eq!(*entries.last().unwrap(), [4, 6, 0xff, 0, 0, 1]);
    }

    #[test]
    fn a_board_without_apics_gets_no_madt() {
        assert!(rsdt_tables(&load()).iter().all(|t| &t[..4] != b"APIC"));
    }

    #[test]
    fn dsdt_declares_the_processor_the_madt_names() {
        assert!(dsdt_contains(&aml::scope("\\_PR", [aml::processor("CPU0", CPU_ID, 0, 0, [])])));
    }

    #[test]
    fn fadt_describes_the_piix4_pm_registers() {
        let f = fadt();
        assert_eq!(f.len(), 116);
        assert_eq!(u32_at(&f, 56), 0x600);
        assert_eq!(u32_at(&f, 64), 0x604);
        assert_eq!(u32_at(&f, 76), 0x608);
        assert_eq!(u32_at(&f, 80), 0x60c);
        assert_eq!(u16::from_le_bytes([f[46], f[47]]), 9);
        assert_eq!(u32_at(&f, 48), 0xb2);
    }

    fn dsdt_contains(needle: &[u8]) -> bool {
        dsdt(512 << 20).windows(needle.len()).any(|w| w == needle)
    }

    fn s5() -> Vec<u8> {
        aml::name("\\_S5", aml::package([aml::int(0), aml::int(0), aml::int(0), aml::int(0)]))
    }

    #[test]
    fn dsdt_declares_s5_as_soft_off() {
        assert!(dsdt_contains(&s5()));
    }

    #[test]
    fn dsdt_declares_the_pci_root_isa_devices_and_power_button() {
        for id in ["PNP0A03", "PNP0303", "PNP0F13", "PNP0000", "PNP0100", "PNP0B00", "PNP0501", "PNP0700", "PNP0C0C", "PNP0C0F"] {
            assert!(dsdt_contains(&aml::name("_HID", aml::eisaid(id))), "{id} missing");
        }
    }

    #[test]
    fn prt_routes_pins_like_the_board() {
        let slot1_intd = aml::package([aml::int(0x1_ffff), aml::int(3), aml::path("LNKD"), aml::int(0)]);
        assert!(dsdt_contains(&slot1_intd));
        let slot2_inta = aml::package([aml::int(0x2_ffff), aml::int(0), aml::path("LNKB"), aml::int(0)]);
        assert!(dsdt_contains(&slot2_inta));
    }

    #[test]
    fn dsdt_top_level_package_lengths_cover_the_whole_body() {
        let t = dsdt(512 << 20);
        let body = &t[HEADER_LEN..];
        let mut i = 0;
        while body[i] == 0x10 {
            let lead = body[i + 1];
            let n = (lead >> 6) as usize;
            let low = if n == 0 { lead & 0x3f } else { lead & 0x0f } as usize;
            let len = (0..n).fold(low, |l, k| l | (body[i + 2 + k] as usize) << (4 + 8 * k));
            i += 1 + len;
        }
        assert_eq!(&body[i..], s5());
    }
}
