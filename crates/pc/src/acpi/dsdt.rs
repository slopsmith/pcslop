//! The DSDT body: the PCI root with its interrupt routing, the legacy ISA
//! devices, the power button, the battery and AC adapter, and the S5 sleep
//! state. The PCI link devices and `_PRT` follow QEMU's i440FX DSDT in
//! `hw/i386/acpi-build.c` and `hw/isa/piix.c` (Igor Mammedov, Michael S.
//! Tsirkin and the QEMU authors). The battery follows the control-method
//! battery of ACPI 1.0b section 11.2.

use super::aml::{self, Access, Aml, RegionSpace, res};
use crate::battery::{self, event, reg, status};
use crate::pci;
use crate::pm;

const LINKS: [&str; 4] = ["LNKA", "LNKB", "LNKC", "LNKD"];
const LINK_IRQS: [u32; 3] = [5, 10, 11];
const PCI_HOLE_END: u32 = 0xfebf_ffff;

pub fn body(ram_bytes: u32, layout: super::Layout) -> Aml {
    [
        aml::scope("\\_PR", [aml::processor("CPU0", super::CPU_ID, 0, 0, [])]),
        aml::scope(
            "\\_SB",
            [
                pci_root(ram_bytes, layout),
                aml::field("PCI0.ISA.P40C", aml::Access::Byte, &[("PRQ0", 8), ("PRQ1", 8), ("PRQ2", 8), ("PRQ3", 8)]),
                link_status_method(),
                link_current_settings_method(),
            ]
            .into_iter()
            .chain((0..4).map(link_device))
            .chain([sci_link(), aml::device("PWRB", [aml::name("_HID", aml::eisaid("PNP0C0C"))])])
            .chain(power_devices()),
        ),
        aml::scope(
            "\\_GPE",
            [
                aml::method(
                    &format!("_E{:02X}", pm::GPE_POWER_BUTTON),
                    0,
                    false,
                    [aml::notify(aml::path("\\_SB.PWRB"), aml::int(0x80))],
                ),
                power_event_method(),
            ],
        ),
        aml::name("\\_S5", aml::package([aml::int(pm::SLP_TYP_SOFF as u64), aml::int(0), aml::int(0), aml::int(0)])),
    ]
    .concat()
}

fn pci_root(ram_bytes: u32, layout: super::Layout) -> Aml {
    let crs = res::template([
        res::word_bus_number(0, 0xff),
        res::io(0xcf8, 8),
        res::word_io(0, 0xcf7),
        res::word_io(0xd00, 0xffff),
        res::dword_memory(0xa_0000, 0xb_ffff),
        res::dword_memory(ram_bytes, PCI_HOLE_END),
    ]);
    aml::device(
        "PCI0",
        [
            aml::name("_HID", aml::eisaid("PNP0A03")),
            aml::name("_ADR", aml::int(0)),
            aml::name("_UID", aml::int(0)),
            aml::name("_CRS", crs),
            aml::method("_PRT", 0, false, [aml::ret(routing_table(layout.piix))]),
            isa_bridge(layout.piix),
        ]
        .into_iter()
        .chain(layout.agp.then(agp_bridge)),
    )
}

/// The AGP bridge at 00:01 and the card behind it, whose pins the board
/// swizzles as if the card sat in slot 1.
fn agp_bridge() -> Aml {
    let prt = aml::package((0..4u8).map(|pin| {
        let link = LINKS[pci::pirq_for(1, pin + 1)];
        aml::package([aml::int(0xffff), aml::int(pin as u64), aml::path(link), aml::int(0)])
    }));
    aml::device("AGPB", [aml::name("_ADR", aml::int(0x0001_0000)), aml::method("_PRT", 0, false, [aml::ret(prt)])])
}

/// One entry per slot and pin, each naming the link its INTx line reaches.
fn routing_table(piix: u8) -> Aml {
    aml::package((0..32u8).flat_map(move |slot| {
        (0..4u8).map(move |pin| {
            let link = if slot == piix && pin == 0 { "LNKS" } else { LINKS[pci::pirq_for(slot, pin + 1)] };
            aml::package([aml::int((slot as u64) << 16 | 0xffff), aml::int(pin as u64), aml::path(link), aml::int(0)])
        })
    }))
}

fn isa_device(name: &str, hid: &str, extra: Vec<Aml>, crs: Vec<Aml>) -> Aml {
    let body = [aml::name("_HID", aml::eisaid(hid))].into_iter().chain(extra).chain([aml::name("_CRS", res::template(crs))]);
    aml::device(name, body)
}

fn isa_bridge(piix: u8) -> Aml {
    aml::device(
        "ISA",
        [
            aml::name("_ADR", aml::int((piix as u64) << 16)),
            aml::op_region("P40C", RegionSpace::PciConfig, pci::PIRQRC as u64, 4),
            isa_device("KBD", "PNP0303", vec![], vec![res::io(0x60, 1), res::io(0x64, 1), res::irq_no_flags(1)]),
            isa_device("MOU", "PNP0F13", vec![], vec![res::irq_no_flags(12)]),
            isa_device("PIC", "PNP0000", vec![], vec![res::io(0x20, 2), res::io(0xa0, 2), res::io(0x4d0, 2), res::irq_no_flags(2)]),
            isa_device("TMR", "PNP0100", vec![], vec![res::io(0x40, 4), res::irq_no_flags(0)]),
            isa_device("RTC", "PNP0B00", vec![], vec![res::io(0x70, 2), res::irq_no_flags(8)]),
            isa_device(
                "COM1",
                "PNP0501",
                vec![aml::name("_UID", aml::int(1))],
                vec![res::io(0x3f8, 8), res::irq_no_flags(4)],
            ),
            isa_device(
                "FDC0",
                "PNP0700",
                vec![],
                vec![res::io(0x3f2, 4), res::io(0x3f7, 1), res::irq_no_flags(6), res::dma(2)],
            ),
        ],
    )
}

/// `IQST(PRQx)`: a PIRQ route register with bit 7 set is a disabled link.
fn link_status_method() -> Aml {
    aml::method(
        "IQST",
        1,
        false,
        [aml::if_(aml::and(aml::int(0x80), aml::arg(0)), [aml::ret(aml::int(0x09))]), aml::ret(aml::int(0x0b))],
    )
}

/// `IQCR(PRQx)`: the link's current IRQ as a resource template.
fn link_current_settings_method() -> Aml {
    aml::method(
        "IQCR",
        1,
        true,
        [
            aml::name("PRR0", res::template([res::ext_irq(&[0])])),
            aml::create_dword_field(aml::path("PRR0"), aml::int(5), "PRRI"),
            aml::if_(aml::lless(aml::arg(0), aml::int(0x80)), [aml::store(aml::arg(0), aml::path("PRRI"))]),
            aml::ret(aml::path("PRR0")),
        ],
    )
}

fn link_device(n: usize) -> Aml {
    let reg = format!("PRQ{n}");
    aml::device(
        LINKS[n],
        [
            aml::name("_HID", aml::eisaid("PNP0C0F")),
            aml::name("_UID", aml::int(n as u64)),
            aml::name("_PRS", res::template([res::ext_irq(&LINK_IRQS)])),
            aml::method("_STA", 0, false, [aml::ret(aml::call("IQST", [aml::path(&reg)]))]),
            aml::method("_DIS", 0, false, [aml::or(aml::path(&reg), aml::int(0x80), aml::path(&reg))]),
            aml::method("_CRS", 0, false, [aml::ret(aml::call("IQCR", [aml::path(&reg)]))]),
            aml::method(
                "_SRS",
                1,
                false,
                [aml::create_dword_field(aml::arg(0), aml::int(5), "PRRI"), aml::store(aml::path("PRRI"), aml::path(&reg))],
            ),
        ],
    )
}

/// The link for the PM function's INTA, which is the SCI on IRQ 9.
fn sci_link() -> Aml {
    aml::device(
        "LNKS",
        [
            aml::name("_HID", aml::eisaid("PNP0C0F")),
            aml::name("_UID", aml::int(4)),
            aml::name("_PRS", res::template([res::ext_irq(&[pm::SCI_IRQ as u32])])),
            aml::method("_STA", 0, false, [aml::ret(aml::int(0x0b))]),
            aml::method("_DIS", 0, false, []),
            aml::method("_CRS", 0, false, [aml::ret(aml::path("_PRS"))]),
            aml::method("_SRS", 1, false, []),
        ],
    )
}

/// (field name, register) in window order, as `Field` needs them.
const POWER_FIELDS: [(&str, u16); 12] = [
    ("PSTS", reg::STATUS),
    ("PEVT", reg::EVENTS),
    ("BSTA", reg::STATE),
    ("BRAT", reg::RATE),
    ("BRCP", reg::REMAINING),
    ("BPVL", reg::VOLTAGE),
    ("BDCP", reg::DESIGN_CAPACITY),
    ("BFCP", reg::FULL_CAPACITY),
    ("BDVL", reg::DESIGN_VOLTAGE),
    ("BWRN", reg::WARNING),
    ("BLOW", reg::LOW),
    ("BGRN", reg::GRANULARITY),
];

const POWER_UNIT_MAH: u64 = 1;
const TECHNOLOGY_RECHARGEABLE: u64 = 1;
const STA_ABSENT_BATTERY: u64 = 0x0f;
const STA_PRESENT_BATTERY: u64 = 0x1f;

/// `Store(field, Index(package, i))` for each (field, i), then `Return(package)`.
fn fill_package(package: &str, fields: &[(&str, u64)]) -> Vec<Aml> {
    let stores = fields.iter().map(|&(f, i)| aml::store(aml::path(f), aml::index(aml::path(package), aml::int(i))));
    stores.chain([aml::ret(aml::path(package))]).collect()
}

fn power_devices() -> Vec<Aml> {
    let fields: Vec<(&str, usize)> = POWER_FIELDS.iter().map(|&(n, _)| (n, 32)).collect();
    let unknown = || aml::int(0xffff_ffff);
    let bif = aml::package([
        aml::int(POWER_UNIT_MAH),
        unknown(),
        unknown(),
        aml::int(TECHNOLOGY_RECHARGEABLE),
        unknown(),
        aml::int(0),
        aml::int(0),
        aml::int(0),
        aml::int(0),
        aml::string("SLOP-1"),
        aml::string("86"),
        aml::string("LION"),
        aml::string("86slopper"),
    ]);
    let bif_fields = [("BDCP", 1), ("BFCP", 2), ("BDVL", 4), ("BWRN", 5), ("BLOW", 6), ("BGRN", 7), ("BGRN", 8)];
    let bst_fields = [("BSTA", 0), ("BRAT", 1), ("BRCP", 2), ("BPVL", 3)];
    let powered = || aml::name("_PCL", aml::package([aml::path("\\_SB")]));
    vec![
        aml::op_region("PWRR", RegionSpace::SystemIo, battery::BASE as u64, battery::LEN as u64),
        aml::field("PWRR", Access::DWord, &fields),
        aml::device(
            "BAT0",
            [
                aml::name("_HID", aml::eisaid("PNP0C0A")),
                aml::name("_UID", aml::int(1)),
                powered(),
                aml::method(
                    "_STA",
                    0,
                    false,
                    [
                        aml::if_(aml::and(aml::path("PSTS"), aml::int(status::BATTERY_PRESENT as u64)), [aml::ret(aml::int(STA_PRESENT_BATTERY))]),
                        aml::ret(aml::int(STA_ABSENT_BATTERY)),
                    ],
                ),
                aml::name("PBIF", bif),
                aml::method("_BIF", 0, true, fill_package("PBIF", &bif_fields)),
                aml::name("PBST", aml::package([aml::int(0), aml::int(0), aml::int(0), aml::int(0)])),
                aml::method("_BST", 0, true, fill_package("PBST", &bst_fields)),
            ],
        ),
        aml::device(
            "AC",
            [
                aml::name("_HID", aml::string("ACPI0003")),
                powered(),
                aml::method(
                    "_PSR",
                    0,
                    false,
                    [
                        aml::if_(aml::and(aml::path("PSTS"), aml::int(status::AC_ONLINE as u64)), [aml::ret(aml::int(1))]),
                        aml::ret(aml::int(0)),
                    ],
                ),
            ],
        ),
    ]
}

/// Acknowledges the pending events in the window and sends each its notification.
fn power_event_method() -> Aml {
    let notify_if = |bit: u32, device: &str, code: u64| {
        aml::if_(aml::and(aml::local(0), aml::int(bit as u64)), [aml::notify(aml::path(device), aml::int(code))])
    };
    aml::method(
        &format!("_E{:02X}", pm::GPE_BATTERY),
        0,
        false,
        [
            aml::store(aml::path("\\_SB.PEVT"), aml::local(0)),
            aml::store(aml::local(0), aml::path("\\_SB.PEVT")),
            notify_if(event::AC, "\\_SB.AC", 0x80),
            notify_if(event::BATTERY_STATUS, "\\_SB.BAT0", 0x80),
            notify_if(event::BATTERY_INFO, "\\_SB.BAT0", 0x81),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn power_fields_cover_the_register_window_in_order() {
        let offsets: Vec<u16> = POWER_FIELDS.iter().map(|&(_, r)| r).collect();
        let expected: Vec<u16> = (0..battery::LEN).step_by(4).collect();
        assert_eq!(offsets, expected);
    }

    #[test]
    fn battery_status_method_matches_iasl() {
        let iasl = hex(
            "143d5f42535408704253544188504253540000704252415488504253540100704252435088504253540a0200704250564c88504253540a0300a450425354",
        );
        assert!(power_devices().concat().windows(iasl.len()).any(|w| w == iasl));
    }

    #[test]
    fn ac_power_source_method_matches_iasl() {
        let iasl = hex("14145f50535200a00b7b505354530a0200a401a400");
        assert!(power_devices().concat().windows(iasl.len()).any(|w| w == iasl));
    }

    #[test]
    fn power_event_handler_matches_iasl() {
        let iasl = hex(concat!(
            "144a055f45303100705c2e5f53425f504556546070605c2e5f53425f50455654",
            "a0127b600100865c2e5f53425f41435f5f0a80",
            "a0137b600a0200865c2e5f53425f424154300a80",
            "a0137b600a0400865c2e5f53425f424154300a81",
        ));
        assert_eq!(power_event_method(), iasl);
    }
}
