//! The DSDT body: the PCI root with its interrupt routing, the legacy ISA
//! devices, the power button and the S5 sleep state. The PCI link devices and
//! `_PRT` follow QEMU's i440FX DSDT in `hw/i386/acpi-build.c` and
//! `hw/isa/piix.c` (Igor Mammedov, Michael S. Tsirkin and the QEMU authors).

use super::aml::{self, Aml, RegionSpace, res};
use crate::pci;
use crate::pm;

const LINKS: [&str; 4] = ["LNKA", "LNKB", "LNKC", "LNKD"];
const LINK_IRQS: [u32; 3] = [5, 10, 11];
const PCI_HOLE_END: u32 = 0xfebf_ffff;

pub fn body(ram_bytes: u32) -> Aml {
    [
        aml::scope("\\_PR", [aml::processor("CPU0", super::CPU_ID, 0, 0, [])]),
        aml::scope(
            "\\_SB",
            [
                pci_root(ram_bytes),
                aml::field("PCI0.ISA.P40C", &[("PRQ0", 8), ("PRQ1", 8), ("PRQ2", 8), ("PRQ3", 8)]),
                link_status_method(),
                link_current_settings_method(),
            ]
            .into_iter()
            .chain((0..4).map(link_device))
            .chain([sci_link(), aml::device("PWRB", [aml::name("_HID", aml::eisaid("PNP0C0C"))])]),
        ),
        aml::scope(
            "\\_GPE",
            [aml::method(
                &format!("_E{:02X}", pm::GPE_POWER_BUTTON),
                0,
                false,
                [aml::notify(aml::path("\\_SB.PWRB"), aml::int(0x80))],
            )],
        ),
        aml::name("\\_S5", aml::package([aml::int(pm::SLP_TYP_SOFF as u64), aml::int(0), aml::int(0), aml::int(0)])),
    ]
    .concat()
}

fn pci_root(ram_bytes: u32) -> Aml {
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
            aml::method("_PRT", 0, false, [aml::ret(routing_table())]),
            isa_bridge(),
        ],
    )
}

/// One entry per slot and pin, each naming the link its INTx line reaches.
fn routing_table() -> Aml {
    aml::package((0..32u8).flat_map(|slot| {
        (0..4u8).map(move |pin| {
            let link = if slot == 1 && pin == 0 { "LNKS" } else { LINKS[pci::pirq_for(slot, pin + 1)] };
            aml::package([aml::int((slot as u64) << 16 | 0xffff), aml::int(pin as u64), aml::path(link), aml::int(0)])
        })
    }))
}

fn isa_device(name: &str, hid: &str, extra: Vec<Aml>, crs: Vec<Aml>) -> Aml {
    let body = [aml::name("_HID", aml::eisaid(hid))].into_iter().chain(extra).chain([aml::name("_CRS", res::template(crs))]);
    aml::device(name, body)
}

fn isa_bridge() -> Aml {
    aml::device(
        "ISA",
        [
            aml::name("_ADR", aml::int(0x0001_0000)),
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
