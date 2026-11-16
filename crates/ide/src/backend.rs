//! Block storage behind the emulated drives.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Byte-addressed storage for a disk or CD image.
pub trait BlockBackend: Send {
    fn len(&self) -> u64;
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()>;
    fn read_only(&self) -> bool;
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A raw image file.
pub struct FileBackend {
    file: File,
    len: u64,
    read_only: bool,
}

impl FileBackend {
    pub fn open(path: impl AsRef<Path>, read_only: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let len = file.metadata()?.len();
        Ok(FileBackend { file, len, read_only })
    }
}

impl BlockBackend for FileBackend {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(buf, off)
    }

    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "read-only image"));
        }
        self.file.write_all_at(buf, off)?;
        self.len = self.len.max(off + buf.len() as u64);
        Ok(())
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.read_only { Ok(()) } else { self.file.sync_data() }
    }
}

/// An image held in memory.
pub struct MemBackend {
    pub data: Vec<u8>,
    pub read_only: bool,
}

impl MemBackend {
    pub fn new(data: Vec<u8>, read_only: bool) -> Self {
        MemBackend { data, read_only }
    }
}

impl BlockBackend for MemBackend {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = usize::try_from(off).map_err(|_| io::ErrorKind::UnexpectedEof)?;
        let src = self.data.get(start..start + buf.len()).ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "read-only image"));
        }
        let start = usize::try_from(off).map_err(|_| io::ErrorKind::UnexpectedEof)?;
        let dst = self.data.get_mut(start..start + buf.len()).ok_or(io::ErrorKind::UnexpectedEof)?;
        dst.copy_from_slice(buf);
        Ok(())
    }

    fn read_only(&self) -> bool {
        self.read_only
    }
}
