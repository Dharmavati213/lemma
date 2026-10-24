//! Windows: poppler-glib's header declares `poppler_certificate_info_new`, but the function lacks
//! `POPPLER_PUBLIC`, so its DLL does not export it. poppler-rs refers to it (from
//! `CertificateInfo::new`), and linkers for Windows report the missing symbol even where nothing
//! calls it, which ELF linkers do not. Lemma never creates a `CertificateInfo`; this stands in.
//!
//! It lives here, in a crate without poppler, because this crate is linked into every program
//! that links poppler-glib (`lemma`, and the tests of `lemma-pdf` and `lemma-preview`), and a
//! second definition elsewhere would clash with it.

/// Never called by Lemma (see the module docs); returns no certificate.
#[unsafe(no_mangle)]
pub extern "C" fn poppler_certificate_info_new() -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}
