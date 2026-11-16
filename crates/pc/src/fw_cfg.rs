//! QEMU's firmware configuration interface, as SeaBIOS expects it: a 16-bit
//! selector at port 0x510 and a byte-wide data port at 0x511. The layout
//! follows QEMU's `docs/specs/fw_cfg.rst`; DMA is not offered.

use std::collections::BTreeMap;

pub const PORT_SELECTOR: u16 = 0x510;
pub const PORT_DATA: u16 = 0x511;

pub mod key {
    pub const SIGNATURE: u16 = 0x00;
    pub const ID: u16 = 0x01;
    pub const RAM_SIZE: u16 = 0x03;
    pub const NOGRAPHIC: u16 = 0x04;
    pub const NB_CPUS: u16 = 0x05;
    pub const MAX_CPUS: u16 = 0x0f;
    pub const FILE_DIR: u16 = 0x19;
    pub const FILE_FIRST: u16 = 0x20;
}

#[derive(Default)]
pub struct FwCfg {
    items: BTreeMap<u16, Vec<u8>>,
    files: Vec<(String, u16)>,
    selected: u16,
    offset: usize,
}

impl FwCfg {
    pub fn new() -> Self {
        let mut f = FwCfg::default();
        f.set(key::SIGNATURE, b"QEMU".to_vec());
        f.set(key::ID, 1u32.to_le_bytes().to_vec());
        f.set(key::NOGRAPHIC, 0u16.to_le_bytes().to_vec());
        f.set(key::NB_CPUS, 1u16.to_le_bytes().to_vec());
        f.set(key::MAX_CPUS, 1u16.to_le_bytes().to_vec());
        f.rebuild_dir();
        f
    }

    pub fn set(&mut self, key: u16, data: Vec<u8>) {
        self.items.insert(key, data);
    }

    pub fn add_file(&mut self, name: &str, data: Vec<u8>) {
        let key = key::FILE_FIRST + self.files.len() as u16;
        self.files.push((name.to_string(), key));
        self.set(key, data);
        self.rebuild_dir();
    }

    fn rebuild_dir(&mut self) {
        let mut dir = (self.files.len() as u32).to_be_bytes().to_vec();
        for (name, key) in &self.files {
            let size = self.items[key].len() as u32;
            dir.extend(size.to_be_bytes());
            dir.extend(key.to_be_bytes());
            dir.extend(0u16.to_be_bytes());
            let mut n = [0u8; 56];
            n[..name.len()].copy_from_slice(name.as_bytes());
            dir.extend(n);
        }
        self.items.insert(key::FILE_DIR, dir);
    }

    pub fn write_selector(&mut self, v: u16) {
        self.selected = v;
        self.offset = 0;
    }

    pub fn read_data(&mut self) -> u8 {
        let b = self.items.get(&self.selected).and_then(|d| d.get(self.offset)).copied().unwrap_or(0);
        self.offset += 1;
        b
    }
}

/// Builds the `etc/e820` file: 20-byte little-endian entries of base, length and type.
pub fn e820(entries: &[(u64, u64, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(base, len, typ) in entries {
        out.extend(base.to_le_bytes());
        out.extend(len.to_le_bytes());
        out.extend(typ.to_le_bytes());
    }
    out
}
