//! The Mac's battery and AC state, as the guest's ACPI battery shows it.
//! IOKit's power-source API gives the charge macOS shows and what powers the
//! Mac; the `AppleSmartBattery` registry entry adds capacities in mAh,
//! voltage and current when it has them.

use std::ffi::{CStr, c_char, c_void};
use std::sync::mpsc::Sender;
use std::time::Duration;

use pc::battery::{Battery, Power};

type CfRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CfRef);
    fn CFGetTypeID(cf: CfRef) -> usize;
    fn CFArrayGetCount(array: CfRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFDictionaryGetValue(dict: CfRef, key: CfRef) -> CfRef;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: CfRef, kind: isize, out: *mut c_void) -> u8;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(boolean: CfRef) -> u8;
    fn CFStringCreateWithBytes(alloc: CfRef, bytes: *const u8, len: isize, encoding: u32, external: u8) -> CfRef;
    fn CFEqual(a: CfRef, b: CfRef) -> u8;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> CfRef;
    fn IOPSCopyPowerSourcesList(blob: CfRef) -> CfRef;
    fn IOPSGetPowerSourceDescription(blob: CfRef, source: CfRef) -> CfRef;
    fn IOPSGetProvidingPowerSourceType(blob: CfRef) -> CfRef;
    fn IOServiceMatching(name: *const c_char) -> CfRef;
    fn IOServiceGetMatchingService(main_port: u32, matching: CfRef) -> u32;
    fn IORegistryEntryCreateCFProperties(entry: u32, properties: *mut CfRef, alloc: CfRef, options: u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
}

const UTF8: u32 = 0x0800_0100;
const NUMBER_SINT64: isize = 4;

struct Cf(CfRef);

impl Cf {
    fn new(r: CfRef) -> Option<Cf> {
        (!r.is_null()).then_some(Cf(r))
    }

    fn string(s: &str) -> Cf {
        Cf(unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, UTF8, 0) })
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

/// A borrowed CFDictionary.
#[derive(Clone, Copy)]
struct Dict(CfRef);

impl Dict {
    fn new(r: CfRef) -> Option<Dict> {
        (!r.is_null() && unsafe { CFGetTypeID(r) == CFDictionaryGetTypeID() }).then_some(Dict(r))
    }

    fn get(self, key: &str) -> Option<CfRef> {
        let k = Cf::string(key);
        let v = unsafe { CFDictionaryGetValue(self.0, k.0) };
        (!v.is_null()).then_some(v)
    }

    fn dict(self, key: &str) -> Option<Dict> {
        self.get(key).and_then(Dict::new)
    }

    fn int(self, key: &str) -> Option<i64> {
        let v = self.get(key)?;
        let mut out = 0i64;
        let ok = unsafe { CFGetTypeID(v) == CFNumberGetTypeID() && CFNumberGetValue(v, NUMBER_SINT64, (&raw mut out).cast()) != 0 };
        ok.then_some(out)
    }

    fn bool(self, key: &str) -> Option<bool> {
        let v = self.get(key)?;
        unsafe { (CFGetTypeID(v) == CFBooleanGetTypeID()).then(|| CFBooleanGetValue(v) != 0) }
    }

    fn is(self, key: &str, value: &str) -> bool {
        self.get(key).is_some_and(|v| unsafe { CFEqual(v, Cf::string(value).0) != 0 })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Source {
    percent: u32,
    charging: bool,
    minutes_to_empty: Option<u32>,
}

/// The `AppleSmartBattery` gauge; the current is negative while discharging.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Gauge {
    design_mah: Option<u32>,
    full_mah: Option<u32>,
    voltage_mv: Option<u32>,
    current_ma: Option<i64>,
}

fn read_sources() -> (bool, Option<Source>) {
    let Some(info) = Cf::new(unsafe { IOPSCopyPowerSourcesInfo() }) else {
        return (true, None);
    };
    let providing = unsafe { IOPSGetProvidingPowerSourceType(info.0) };
    let on_ac = providing.is_null() || unsafe { CFEqual(providing, Cf::string("AC Power").0) != 0 };
    let Some(list) = Cf::new(unsafe { IOPSCopyPowerSourcesList(info.0) }) else {
        return (on_ac, None);
    };
    let battery = (0..unsafe { CFArrayGetCount(list.0) })
        .filter_map(|i| Dict::new(unsafe { IOPSGetPowerSourceDescription(info.0, CFArrayGetValueAtIndex(list.0, i)) }))
        .find(|d| d.is("Type", "InternalBattery") && d.bool("Is Present") != Some(false));
    let source = battery.map(|d| {
        let current = d.int("Current Capacity").unwrap_or(0);
        let max = d.int("Max Capacity").filter(|&m| m > 0).unwrap_or(100);
        Source {
            percent: (current * 100 / max).clamp(0, 100) as u32,
            charging: d.bool("Is Charging").unwrap_or(false),
            minutes_to_empty: d.int("Time to Empty").filter(|&t| t > 0).map(|t| t as u32),
        }
    });
    (on_ac, source)
}

fn read_gauge() -> Gauge {
    const NAME: &CStr = c"AppleSmartBattery";
    let entry = unsafe { IOServiceGetMatchingService(0, IOServiceMatching(NAME.as_ptr())) };
    if entry == 0 {
        return Gauge::default();
    }
    let mut props: CfRef = std::ptr::null();
    let status = unsafe { IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null(), 0) };
    unsafe { IOObjectRelease(entry) };
    let Some(props) = (status == 0).then(|| Cf::new(props)).flatten() else {
        return Gauge::default();
    };
    let Some(top) = Dict::new(props.0) else {
        return Gauge::default();
    };
    let data = top.dict("BatteryData");
    let positive = |key: &str| {
        let v = top.int(key).or_else(|| data.and_then(|d| d.int(key)))?;
        u32::try_from(v).ok().filter(|&v| v > 0)
    };
    Gauge {
        design_mah: positive("DesignCapacity"),
        full_mah: positive("AppleRawMaxCapacity").or_else(|| positive("NominalChargeCapacity")),
        voltage_mv: positive("Voltage"),
        current_ma: top.int("Amperage"),
    }
}

const CELL_MAX_MV: u32 = 4400;
const CELL_NOMINAL_MV: u32 = 3800;
const FALLBACK_MAH: u32 = 5000;

// Remaining capacity follows the percentage macOS shows, not the raw gauge.
// macOS publishes no design voltage, so it is guessed from the cells in series.
fn to_power(on_ac: bool, source: Option<Source>, gauge: Gauge) -> Power {
    let Some(s) = source else {
        return Power { ac_online: true, battery: None };
    };
    let full_mah = gauge.full_mah.or(gauge.design_mah).unwrap_or(FALLBACK_MAH);
    let remaining_mah = (full_mah as u64 * s.percent as u64).div_ceil(100) as u32;
    let design_mv = gauge.voltage_mv.map_or(3 * CELL_NOMINAL_MV, |v| v.div_ceil(CELL_MAX_MV).max(1) * CELL_NOMINAL_MV);
    let derived_rate = || match s.minutes_to_empty {
        Some(m) if !on_ac => Some((remaining_mah as u64 * 60 / m as u64) as u32),
        _ => None,
    };
    let rate_ma = gauge.current_ma.map(|c| c.unsigned_abs().min(0x7fff_ffff) as u32).or_else(derived_rate);
    Power {
        ac_online: on_ac,
        battery: Some(Battery {
            design_mah: gauge.design_mah.unwrap_or(full_mah),
            full_mah,
            remaining_mah,
            design_mv,
            present_mv: gauge.voltage_mv.unwrap_or(design_mv),
            rate_ma,
            charging: s.charging && on_ac,
        }),
    }
}

/// The host's power state now. Takes a few milliseconds of IOKit calls.
pub fn read() -> Power {
    let (on_ac, source) = read_sources();
    let gauge = if source.is_some() { read_gauge() } else { Gauge::default() };
    to_power(on_ac, source, gauge)
}

/// Sends a reading of the host every `every` from a thread of its own, until
/// the receiver is gone.
pub fn poll<E: Send + 'static>(every: Duration, tx: Sender<E>, wrap: fn(Power) -> E) {
    std::thread::Builder::new()
        .name("host-power".into())
        .spawn(move || {
            while tx.send(wrap(read())).is_ok() {
                std::thread::sleep(every);
            }
        })
        .expect("cannot start the host power thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAUGE: Gauge = Gauge { design_mah: Some(8579), full_mah: Some(7014), voltage_mv: Some(12_244), current_ma: Some(-1500) };

    fn source(percent: u32, charging: bool) -> Source {
        Source { percent, charging, minutes_to_empty: None }
    }

    #[test]
    fn a_mac_without_a_battery_runs_on_ac() {
        assert_eq!(to_power(false, None, Gauge::default()), Power { ac_online: true, battery: None });
    }

    #[test]
    fn remaining_capacity_follows_the_shown_percentage() {
        let b = to_power(false, Some(source(80, false)), GAUGE).battery.unwrap();
        assert_eq!((b.design_mah, b.full_mah, b.remaining_mah), (8579, 7014, 5612));
        assert_eq!(b.percent(), 80);
    }

    #[test]
    fn current_is_reported_as_an_unsigned_rate() {
        let b = to_power(false, Some(source(80, false)), GAUGE).battery.unwrap();
        assert_eq!(b.rate_ma, Some(1500));
    }

    #[test]
    fn design_voltage_is_the_nominal_voltage_of_the_cell_count() {
        let at = |mv| to_power(true, Some(source(50, false)), Gauge { voltage_mv: Some(mv), ..GAUGE }).battery.unwrap().design_mv;
        assert_eq!(at(9_100), 11_400);
        assert_eq!(at(13_100), 11_400);
        assert_eq!(at(8_200), 7_600);
    }

    #[test]
    fn without_a_gauge_the_rate_comes_from_time_to_empty() {
        let s = Source { percent: 50, charging: false, minutes_to_empty: Some(150) };
        let b = to_power(false, Some(s), Gauge::default()).battery.unwrap();
        assert_eq!((b.full_mah, b.remaining_mah, b.rate_ma), (FALLBACK_MAH, 2500, Some(1000)));
    }

    #[test]
    fn a_battery_cannot_charge_off_ac() {
        let b = to_power(false, Some(source(50, true)), GAUGE).battery.unwrap();
        assert!(!b.charging);
    }

    #[test]
    fn reading_this_host_gives_a_consistent_battery() {
        if let Some(b) = read().battery {
            assert!(b.remaining_mah <= b.full_mah);
        }
    }
}
