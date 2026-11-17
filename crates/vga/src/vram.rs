use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ops::{Deref, DerefMut};

/// Zeroed video memory aligned to the host page size (16 KiB on Apple
/// Silicon), so a hypervisor can map it into a guest.
pub struct Vram {
    ptr: *mut u8,
    len: usize,
}

const ALIGN: usize = 16 << 10;

impl Vram {
    pub fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len, ALIGN).unwrap();
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "out of memory for VRAM");
        Vram { ptr, len }
    }
}

impl Drop for Vram {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, Layout::from_size_align(self.len, ALIGN).unwrap()) };
    }
}

impl Deref for Vram {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for Vram {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

unsafe impl Send for Vram {}
