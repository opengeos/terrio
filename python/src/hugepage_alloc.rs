//! The extension's allocator: the system allocator, with buffers of 4 MiB or more advised to use
//! transparent huge pages, as NumPy advises its arrays. Under THP's `madvise` mode (the default of
//! some distributions), a raster buffer otherwise takes a page fault every 4 KiB on first write:
//! filling 170 MB took 0.048 s with 4 KiB pages and 0.019 s with huge pages.

use std::alloc::{GlobalAlloc, Layout, System};

struct HugePageAlloc;

#[global_allocator]
static GLOBAL: HugePageAlloc = HugePageAlloc;

/// The smallest buffer advised.
const MIN_SIZE: usize = 4 << 20;

/// Advises the whole pages of `size` bytes from `p` to use huge pages.
fn advise(p: *mut u8, size: usize) {
    if p.is_null() || size < MIN_SIZE {
        return;
    }
    // SAFETY: sysconf reads a constant of the system (glibc takes it from the auxiliary vector).
    let page = match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        n if n > 0 && (n as usize).is_power_of_two() => n as usize,
        _ => return,
    };
    let start = (p as usize).next_multiple_of(page);
    let end = (p as usize + size) & !(page - 1);
    if end <= start {
        return;
    }
    // SAFETY: the range is whole pages of a live allocation; the advice changes how the kernel
    // backs them, not their contents. An error (no THP support) leaves them as they were.
    unsafe { libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_HUGEPAGE) };
}

// SAFETY: every call is forwarded to `System`, which upholds `GlobalAlloc`'s contract.
unsafe impl GlobalAlloc for HugePageAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's guarantees.
        let p = unsafe { System.alloc(layout) };
        advise(p, layout.size());
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's guarantees.
        let p = unsafe { System.alloc_zeroed(layout) };
        advise(p, layout.size());
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded with the caller's guarantees.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        advise(p, new_size);
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // A null pointer is ignored, as C's `free` ignores it: openjp2 0.6 deallocates a code
        // block's `decoded_data` whether or not it was allocated, and `System::dealloc`'s
        // precondition check aborts on null in a debug build.
        if ptr.is_null() {
            return;
        }
        // SAFETY: forwarded with the caller's guarantees.
        unsafe { System.dealloc(ptr, layout) }
    }
}
