//! Giving freed heap memory back to the system from the PDF thread, after [`PdfCmd::Trim`]
//! freed the SyncTeX scanner there (the app's own trim runs on the main thread before that).
//! The same calls as the app's `heap::trim` for each platform's allocator.
//!
//! [`PdfCmd::Trim`]: crate::PdfCmd::Trim

/// Returns the allocator's free pages to the system.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn trim() {
    // SAFETY: malloc_trim only returns free heap pages to the system.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Returns the allocator's free pages to the system.
#[cfg(target_os = "macos")]
pub(crate) fn trim() {
    // SAFETY: with a null zone and a goal of 0, malloc_zone_pressure_relief only returns the free
    // pages of every malloc zone to the system.
    unsafe {
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

/// Nothing to trim for this allocator.
#[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")))]
pub(crate) fn trim() {}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `<malloc/malloc.h>`: `size_t malloc_zone_pressure_relief(malloc_zone_t *zone, size_t goal)`.
    /// A null `zone` means every zone; a `goal` of 0 means as much as possible.
    fn malloc_zone_pressure_relief(zone: *mut libc::c_void, goal: libc::size_t) -> libc::size_t;
}
