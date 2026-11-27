//! Keeps heap allocations from sharing a cache line. Each Lua VM runs on its own
//! worker and writes its own data constantly, and the system allocator can place data
//! of different VMs next to each other, which slowed scripts several times over
//! depending on where it landed at each start. The cost is memory, see section 2.4 of docs/usage.md.
//!
//! The memory comes from jemalloc. On macOS the system allocator kept the regions that
//! aligned allocations had used and did not use them again, so a server under steady
//! load grew without end (gigabytes within a minute); on Linux it was slower.
//!
//! On FreeBSD jemalloc must be built to leave the C library's thread hooks alone, which
//! `.cargo/config.toml` sets and [`check_c_library_hooks`] verifies.

use std::alloc::{GlobalAlloc, Layout};
use std::ffi::CStr;
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

/// What FreeBSD's C library calls when a thread ends and around a fork. jemalloc built with its
/// defaults defines them itself, so the C library's own jemalloc is never told that a thread
/// ended, and a later thread crashes in it.
const C_LIBRARY_HOOKS: [&CStr; 3] = [c"_malloc_thread_cleanup", c"_malloc_prefork", c"_malloc_postfork"];

/// Fails on FreeBSD when this executable provides any of the C library's thread hooks, as it does
/// when jemalloc was built without the settings of `.cargo/config.toml`. Elsewhere, and where
/// it cannot tell which object provides a hook, it succeeds.
pub fn check_c_library_hooks() -> Result<(), String> {
    judge_hooks(own_object(), &C_LIBRARY_HOOKS.map(|name| (name, object_providing(name))))
}

/// `own` is the loaded object holding this program's code; each hook comes with the object that
/// provides it. Objects are told apart by the address they are loaded at.
fn judge_hooks(own: Option<usize>, hooks: &[(&CStr, Option<usize>)]) -> Result<(), String> {
    let replaced: Vec<_> = hooks.iter().filter(|(_, object)| own.is_some() && *object == own).map(|(name, _)| name.to_string_lossy()).collect();
    if replaced.is_empty() {
        return Ok(());
    }
    Err(format!(
        "this program replaces the C library's {} and would crash after threads end: it was built without \
         ac_cv_func__malloc_thread_cleanup=no and ac_cv_func__pthread_mutex_init_calloc_cb=no; set both \
         (the repository's .cargo/config.toml does), run `cargo clean -p tikv-jemalloc-sys` \
         (release build: `cargo clean --release -p tikv-jemalloc-sys`) and build again",
        replaced.join(", ")
    ))
}

#[cfg(target_os = "freebsd")]
fn object_at(address: *const libc::c_void) -> Option<usize> {
    // SAFETY: `Dl_info` holds only pointers, for which all zeroes is a value.
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: `dladdr` compares the address with the loaded objects and writes `info`.
    (unsafe { libc::dladdr(address, &mut info) } != 0).then_some(info.dli_fbase as usize)
}

#[cfg(target_os = "freebsd")]
fn own_object() -> Option<usize> {
    object_at(own_object as *const libc::c_void)
}

#[cfg(target_os = "freebsd")]
fn object_providing(name: &CStr) -> Option<usize> {
    // SAFETY: `name` ends in NUL.
    let address = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    if address.is_null() {
        return None;
    }
    object_at(address)
}

#[cfg(not(target_os = "freebsd"))]
fn own_object() -> Option<usize> {
    None
}

#[cfg(not(target_os = "freebsd"))]
fn object_providing(_name: &CStr) -> Option<usize> {
    None
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

    const OWN: usize = 0x1000;
    const C_LIBRARY: usize = 0x2000;

    /// The three hooks, those named in `own` provided by the executable.
    fn hooks_with(own: &[&CStr]) -> [(&'static CStr, Option<usize>); 3] {
        C_LIBRARY_HOOKS.map(|name| (name, Some(if own.contains(&name) { OWN } else { C_LIBRARY })))
    }

    // spectra-scenario: lua-auth-engine / Build without the settings / which provider stops the program
    #[test]
    fn test_hooks_left_to_the_c_library() {
        assert_eq!(judge_hooks(Some(OWN), &hooks_with(&[])), Ok(()));
    }

    // spectra-scenario: lua-auth-engine / Build without the settings / which provider stops the program
    #[test]
    fn test_replaced_hooks_are_named() -> Result<(), String> {
        let cases: [(&[&CStr], &str); 2] = [
            (&[c"_malloc_thread_cleanup"], "the C library's _malloc_thread_cleanup and would crash"),
            (&[c"_malloc_prefork", c"_malloc_postfork"], "the C library's _malloc_prefork, _malloc_postfork and would crash"),
        ];
        for (own, named) in cases {
            let Err(message) = judge_hooks(Some(OWN), &hooks_with(own)) else {
                return Err(format!("accepted {own:?}"));
            };
            for part in [named, "ac_cv_func__malloc_thread_cleanup=no", "ac_cv_func__pthread_mutex_init_calloc_cb=no", "`cargo clean -p tikv-jemalloc-sys`", "`cargo clean --release -p tikv-jemalloc-sys`"] {
                assert!(message.contains(part), "{part:?} is not in {message:?}");
            }
            assert!(!message.contains('\n'), "{message:?}");
        }
        Ok(())
    }

    #[test]
    fn test_unknown_providers_do_not_stop_the_program() {
        // A hook that is not found, and a program whose own object is not known.
        let missing = C_LIBRARY_HOOKS.map(|name| (name, None));
        assert_eq!(judge_hooks(Some(OWN), &missing), Ok(()));
        assert_eq!(judge_hooks(None, &missing), Ok(()));
        assert_eq!(judge_hooks(None, &hooks_with(&[c"_malloc_prefork"])), Ok(()));
    }

    // On FreeBSD this looks at the test program itself, built with the repository's settings.
    // spectra-scenario: lua-auth-engine / Build with the repository settings
    #[test]
    fn test_this_build_leaves_the_c_library_hooks() {
        assert_eq!(check_c_library_hooks(), Ok(()));
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
