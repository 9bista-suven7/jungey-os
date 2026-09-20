//! Measured boot.
//!
//! Before the first userspace process exists, the kernel hashes the image it
//! is about to run and compares that against a digest recorded when the two
//! were built together. A single altered byte anywhere in the image changes
//! the digest, and the kernel refuses to start it.
//!
//! **This is measured boot, not verified boot, and the difference matters.** A
//! measurement says "this is the image that was here when the kernel was
//! built". A *signature* says "somebody holding a key I trust vouched for this
//! image", and it is what lets an image be replaced by its author after the
//! fact. The expected digest here lives inside the kernel image, so an
//! attacker who can replace the kernel can replace the digest with it and the
//! check is worth nothing. Closing that needs a root of trust the kernel
//! cannot reach and cannot be talked out of: a boot ROM that checks the kernel
//! against a fused public key, and a key store that will not hand out the
//! private half. QEMU's `virt` machine has neither, and pretending otherwise
//! would be worse than saying so.
//!
//! What *is* real here: the hash, the comparison, the refusal, and the fact
//! that the measurement is recorded where it can be read afterwards. Swapping
//! the digest comparison for a signature check is a contained change to this
//! file once there is hardware to hold the key.

use crate::sha256::{self, Sha256};

extern "C" {
    static __text_start: u8;
    static __rodata_end: u8;
}

/// The kernel's own code and constants, as bytes.
///
/// Reported, not checked: the expected value would have to live inside the
/// region it covers. Measuring yourself and approving of the result is what a
/// boot ROM is for.
pub fn kernel_image() -> &'static [u8] {
    unsafe {
        let start = &__text_start as *const u8;
        let end = &__rodata_end as *const u8;
        core::slice::from_raw_parts(start, end as usize - start as usize)
    }
}

pub struct Boot {
    pub kernel: [u8; 32],
    pub init: [u8; 32],
    /// None when the build-time constant is malformed, which fails the check
    /// rather than matching nothing.
    pub expected: Option<[u8; 32]>,
    pub ok: bool,
}

/// Measure the kernel and the image it is about to run.
pub fn measure(init: &[u8]) -> Boot {
    let kernel = sha256::digest(kernel_image());
    let init = sha256::digest(init);
    let expected = sha256::from_hex(env!("JUNGEY_INIT_SHA256"));
    Boot { kernel, init, expected, ok: expected == Some(init) }
}

/// Hash the image with one byte flipped, without copying it.
///
/// A check that has never been shown to reject anything is a claim rather than
/// a property — the same argument as the action log's tamper test, and it is
/// cheap to answer here: hash the prefix, the altered byte, and the suffix.
pub fn measure_with_flipped_byte(init: &[u8], at: usize) -> [u8; 32] {
    let at = at.min(init.len().saturating_sub(1));
    let mut h = Sha256::new();
    h.update(&init[..at]);
    h.update(&[init[at] ^ 0xff]);
    h.update(&init[at + 1..]);
    h.finish()
}

pub fn short(d: &[u8; 32]) -> alloc::string::String {
    let h = sha256::hex(d);
    let s = core::str::from_utf8(&h).unwrap_or("?");
    alloc::format!("{}…{}", &s[..16], &s[56..])
}

pub fn full(d: &[u8; 32]) -> alloc::string::String {
    let h = sha256::hex(d);
    alloc::string::String::from(core::str::from_utf8(&h).unwrap_or("?"))
}
