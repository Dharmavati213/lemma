//! Giving freed heap memory back to the system (ARCHITECTURE §7), for each platform's allocator.
//!
//! - glibc (Linux): fewer arenas and a fixed mmap threshold ([`tune`]), and `malloc_trim`
//!   ([`trim`]).
//! - macOS: `malloc_zone_pressure_relief` on every zone ([`trim`]); [`tune`] does nothing.
//! - Elsewhere both do nothing.

/// Fewer glibc arenas and an mmap threshold of 1 MiB keep freed memory returnable to the OS.
/// Call it first thing in `main`, before any other thread exists.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn tune() {
    // SAFETY: mallopt only changes allocator tunables; `main` calls this before any other thread
    // exists.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 1 << 20);
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}

/// Nothing to tune for this allocator.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn tune() {}

/// Returns the allocator's free pages to the system.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn trim() {
    // SAFETY: malloc_trim only returns free heap pages to the system.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Returns the allocator's free pages to the system.
#[cfg(target_os = "macos")]
pub fn trim() {
    // SAFETY: with a null zone and a goal of 0, malloc_zone_pressure_relief only returns the free
    // pages of every malloc zone to the system.
    unsafe {
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

/// Nothing to trim for this allocator.
#[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")))]
pub fn trim() {}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `<malloc/malloc.h>`: `size_t malloc_zone_pressure_relief(malloc_zone_t *zone, size_t goal)`.
    /// A null `zone` means every zone; a `goal` of 0 means as much as possible.
    fn malloc_zone_pressure_relief(zone: *mut libc::c_void, goal: libc::size_t) -> libc::size_t;
}
