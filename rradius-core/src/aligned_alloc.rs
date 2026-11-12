//! Keeps heap allocations from sharing a cache line. Each Lua VM runs on its own
//! worker and writes its own data constantly, and the system allocator can place data
//! of different VMs next to each other, which slowed scripts several times over
//! depending on where it landed at each start. The cost is memory, see section 2.4 of docs/usage.md.
//!
//! The memory comes from jemalloc. On macOS the system allocator kept the regions that
//! aligned allocations had used and did not use them again, so a server under steady
//! load grew without end (gigabytes within a minute); on Linux it was slower.

use std::alloc::{GlobalAlloc, Layout};
use tikv_jemallocator::Jemalloc as Backing;

pub const CACHE_LINE: usize = if cfg!(all(target_os = "macos", target_arch = "aarch64")) { 128 } else { 64 };

pub struct CacheLineAligned;

/// What to ask the system for: a whole number of lines, starting on a line.
fn on_whole_lines(layout: Layout) -> Option<Layout> {
    Some(layout.align_to(CACHE_LINE).ok()?.pad_to_align())
}

unsafe impl GlobalAlloc for CacheLineAligned {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match on_whole_lines(layout) {
            Some(lines) => unsafe { Backing.alloc(lines) },
            None => std::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // `alloc` succeeded with this layout, so it cannot overflow here.
        if let Some(lines) = on_whole_lines(layout) {
            unsafe { Backing.dealloc(ptr, lines) }
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        match on_whole_lines(layout) {
            Some(lines) => unsafe { Backing.alloc_zeroed(lines) },
            None => std::ptr::null_mut(),
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = Layout::from_size_align(new_size, layout.align()).ok().and_then(on_whole_lines);
        match (on_whole_lines(layout), new) {
            (Some(lines), Some(new)) => unsafe { Backing.realloc(ptr, lines, new.size()) },
            _ => std::ptr::null_mut(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_aligned(ptr: *mut u8, align: usize) -> bool {
        (ptr as usize).is_multiple_of(align)
    }

    #[test]
    fn test_allocation_alignment() {
        for (size, align) in [(1, 1), (129, 8), (16, 4096)] {
            let layout = Layout::from_size_align(size, align).unwrap();
            // Several, so that one landing on a boundary by chance proves nothing.
            let allocations: Vec<*mut u8> = (0..64).map(|_| unsafe { CacheLineAligned.alloc(layout) }).collect();
            for &ptr in &allocations {
                assert!(!ptr.is_null());
                assert!(is_aligned(ptr, align.max(CACHE_LINE)), "{ptr:?} for size {size}, align {align}");
                unsafe { ptr.write_bytes(0xAB, size) };
            }
            for ptr in allocations {
                unsafe { CacheLineAligned.dealloc(ptr, layout) };
            }
        }
    }

    #[test]
    fn test_small_allocations_do_not_share_a_line() {
        let layout = Layout::from_size_align(1, 1).unwrap();
        let mut allocations: Vec<usize> = (0..64).map(|_| unsafe { CacheLineAligned.alloc(layout) } as usize).collect();
        allocations.sort_unstable();
        for pair in allocations.windows(2) {
            assert!(pair[1] - pair[0] >= CACHE_LINE, "{:#x} and {:#x} share a line", pair[0], pair[1]);
        }
        for address in allocations {
            unsafe { CacheLineAligned.dealloc(address as *mut u8, layout) };
        }
    }

    #[test]
    fn test_resizing_keeps_contents_and_alignment() {
        let layout = Layout::from_size_align(16, 1).unwrap();
        unsafe {
            let ptr = CacheLineAligned.alloc(layout);
            for i in 0..16 {
                ptr.add(i).write(i as u8);
            }
            let grown = CacheLineAligned.realloc(ptr, layout, 1000);
            assert!(!grown.is_null());
            assert!(is_aligned(grown, CACHE_LINE), "{grown:?}");
            for i in 0..16 {
                assert_eq!(grown.add(i).read(), i as u8);
            }
            grown.add(999).write(1);
            CacheLineAligned.dealloc(grown, Layout::from_size_align(1000, 1).unwrap());
        }
    }

    #[test]
    fn test_overflow_returns_null() {
        // Valid as requested; rounding it up to whole lines would overflow.
        let huge = Layout::from_size_align(isize::MAX as usize, 1).unwrap();
        let small = Layout::from_size_align(16, 1).unwrap();
        unsafe {
            assert!(CacheLineAligned.alloc(huge).is_null());
            assert!(CacheLineAligned.alloc_zeroed(huge).is_null());
            let ptr = CacheLineAligned.alloc(small);
            ptr.write(7);
            assert!(CacheLineAligned.realloc(ptr, small, isize::MAX as usize).is_null());
            // A failed resize leaves the original allocation in place.
            assert_eq!(ptr.read(), 7);
            CacheLineAligned.dealloc(ptr, small);
        }
    }

    #[test]
    fn test_cache_line_by_target() {
        let expected = if cfg!(all(target_os = "macos", target_arch = "aarch64")) { 128 } else { 64 };
        assert_eq!(CACHE_LINE, expected);
    }

    #[test]
    fn test_zeroed_allocation() {
        let layout = Layout::from_size_align(256, 1).unwrap();
        unsafe {
            let ptr = CacheLineAligned.alloc_zeroed(layout);
            assert!(!ptr.is_null());
            assert!(is_aligned(ptr, CACHE_LINE), "{ptr:?}");
            assert!(std::slice::from_raw_parts(ptr, 256).iter().all(|&byte| byte == 0));
            CacheLineAligned.dealloc(ptr, layout);
        }
    }
}
