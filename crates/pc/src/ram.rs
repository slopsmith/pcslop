//! Guest RAM in a page-aligned host allocation, so the same memory can be
//! mapped into a Hypervisor.framework VM.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ops::{Deref, DerefMut};

/// Alignment that satisfies both 4 KiB and 16 KiB host page sizes.
const ALIGN: usize = 16 << 10;

pub struct Ram {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for Ram {}

impl Ram {
    pub fn new(len: usize) -> Ram {
        let len = len.next_multiple_of(ALIGN);
        let layout = Layout::from_size_align(len, ALIGN).unwrap();
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "out of memory allocating guest RAM");
        Ram { ptr, len }
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }
}

impl Deref for Ram {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for Ram {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Ram {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, Layout::from_size_align(self.len, ALIGN).unwrap()) }
    }
}
