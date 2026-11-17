//! QEMU's BIOS linker/loader script (`etc/table-loader`): commands that tell
//! the firmware to place fw_cfg files in memory, patch pointers between them
//! and fix up checksums. The format follows QEMU's
//! `hw/acpi/bios-linker-loader.c` (Michael S. Tsirkin, Red Hat), and SeaBIOS
//! runs it in `src/fw/romfile_loader.c`.

const ENTRY_LEN: usize = 128;
const FILE_LEN: usize = 56;

const ALLOCATE: u32 = 1;
const ADD_POINTER: u32 = 2;
const ADD_CHECKSUM: u32 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Zone {
    /// Anywhere in RAM, reserved in the E820 map.
    High = 1,
    /// The 0xF0000 BIOS segment, where the OS scans for the RSDP.
    FSeg = 2,
}

#[derive(Default)]
pub struct Loader {
    script: Vec<u8>,
}

impl Loader {
    fn entry(&mut self, command: u32, build: impl FnOnce(&mut Vec<u8>)) {
        let mut e = command.to_le_bytes().to_vec();
        build(&mut e);
        assert!(e.len() <= ENTRY_LEN);
        e.resize(ENTRY_LEN, 0);
        self.script.extend(e);
    }

    /// Loads `file` into memory from `zone`, aligned to `align` bytes.
    pub fn allocate(&mut self, file: &str, align: u32, zone: Zone) {
        assert!(align.is_power_of_two());
        self.entry(ALLOCATE, |e| {
            e.extend(file_name(file));
            e.extend(align.to_le_bytes());
            e.push(zone as u8);
        });
    }

    /// Adds the address of `src` to the `size`-byte little-endian value at
    /// `offset` in `dest`, so a value holding an offset into `src` becomes a pointer.
    pub fn add_pointer(&mut self, dest: &str, offset: u32, size: u8, src: &str) {
        assert!(matches!(size, 1 | 2 | 4 | 8));
        self.entry(ADD_POINTER, |e| {
            e.extend(file_name(dest));
            e.extend(file_name(src));
            e.extend(offset.to_le_bytes());
            e.push(size);
        });
    }

    /// Makes the bytes `start..start + len` of `file` sum to zero by adjusting
    /// the byte at `offset`, which must start out zero.
    pub fn add_checksum(&mut self, file: &str, offset: u32, start: u32, len: u32) {
        self.entry(ADD_CHECKSUM, |e| {
            e.extend(file_name(file));
            e.extend(offset.to_le_bytes());
            e.extend(start.to_le_bytes());
            e.extend(len.to_le_bytes());
        });
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.script
    }
}

fn file_name(name: &str) -> [u8; FILE_LEN] {
    assert!(name.len() < FILE_LEN);
    let mut out = [0; FILE_LEN];
    out[..name.len()].copy_from_slice(name.as_bytes());
    out
}

/// Runs a loader script the way SeaBIOS does, placing each file at the
/// address `place` picks for it. Returns each file's address and final bytes.
#[cfg(test)]
pub fn execute(
    script: &[u8],
    files: &[(&str, Vec<u8>)],
    mut place: impl FnMut(&str, u32, Zone) -> u32,
) -> std::collections::BTreeMap<String, (u32, Vec<u8>)> {
    use std::collections::BTreeMap;
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let name_at = |b: &[u8], o: usize| {
        let raw = &b[o..o + FILE_LEN];
        String::from_utf8(raw[..raw.iter().position(|&c| c == 0).unwrap()].to_vec()).unwrap()
    };
    let mut loaded: BTreeMap<String, (u32, Vec<u8>)> = BTreeMap::new();
    assert_eq!(script.len() % ENTRY_LEN, 0);
    for e in script.chunks(ENTRY_LEN) {
        match u32_at(e, 0) {
            ALLOCATE => {
                let name = name_at(e, 4);
                let zone = if e[64] == Zone::FSeg as u8 { Zone::FSeg } else { Zone::High };
                let addr = place(&name, u32_at(e, 60), zone);
                let data = files.iter().find(|(n, _)| *n == name).expect("allocated file exists").1.clone();
                assert!(loaded.insert(name, (addr, data)).is_none(), "file allocated twice");
            }
            ADD_POINTER => {
                let src_addr = loaded[&name_at(e, 4 + FILE_LEN)].0 as u64;
                let (offset, size) = (u32_at(e, 4 + 2 * FILE_LEN) as usize, e[8 + 2 * FILE_LEN] as usize);
                let dest = &mut loaded.get_mut(&name_at(e, 4)).expect("pointer into a loaded file").1;
                let mut v = [0u8; 8];
                v[..size].copy_from_slice(&dest[offset..offset + size]);
                let patched = u64::from_le_bytes(v).wrapping_add(src_addr);
                dest[offset..offset + size].copy_from_slice(&patched.to_le_bytes()[..size]);
            }
            ADD_CHECKSUM => {
                let data = &mut loaded.get_mut(&name_at(e, 4)).expect("checksum of a loaded file").1;
                let (offset, start, len) = (u32_at(e, 60) as usize, u32_at(e, 64) as usize, u32_at(e, 68) as usize);
                let sum = data[start..start + len].iter().fold(0u8, |a, &b| a.wrapping_add(b));
                data[offset] = data[offset].wrapping_sub(sum);
            }
            c => panic!("unknown loader command {c}"),
        }
    }
    loaded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_128_bytes_with_the_command_first() {
        let mut l = Loader::default();
        l.allocate("etc/acpi/rsdp", 16, Zone::FSeg);
        let s = l.into_bytes();
        assert_eq!(s.len(), 128);
        assert_eq!(&s[..4], [1, 0, 0, 0]);
        assert_eq!(&s[4..17], b"etc/acpi/rsdp");
        assert_eq!(s[17], 0);
        assert_eq!(&s[60..64], [16, 0, 0, 0]);
        assert_eq!(s[64], 2);
    }

    #[test]
    fn pointer_entry_names_both_files_then_offset_and_size() {
        let mut l = Loader::default();
        l.add_pointer("a", 0x24, 4, "b");
        let s = l.into_bytes();
        assert_eq!(&s[..4], [2, 0, 0, 0]);
        assert_eq!(s[4], b'a');
        assert_eq!(s[60], b'b');
        assert_eq!(&s[116..120], [0x24, 0, 0, 0]);
        assert_eq!(s[120], 4);
    }

    #[test]
    fn executing_patches_pointers_and_zeroes_checksums() {
        let mut l = Loader::default();
        l.allocate("t", 64, Zone::High);
        l.allocate("p", 16, Zone::FSeg);
        l.add_pointer("p", 0, 4, "t");
        l.add_checksum("p", 4, 0, 8);
        let files = [("t", vec![0; 4]), ("p", vec![0x10, 0, 0, 0, 0, 7, 7, 7])];
        let out = execute(&l.into_bytes(), &files, |n, _, _| if n == "t" { 0x1000_0000 } else { 0xf_0000 });
        let p = &out["p"].1;
        assert_eq!(&p[..4], 0x1000_0010u32.to_le_bytes());
        assert_eq!(p.iter().fold(0u8, |a, &b| a.wrapping_add(b)), 0);
    }

    #[test]
    #[should_panic]
    fn file_names_must_fit_the_entry() {
        file_name(&"x".repeat(56));
    }
}
