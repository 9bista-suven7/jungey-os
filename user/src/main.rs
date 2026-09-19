//! Jungey OS userspace test programs.
//!
//! One binary, three roles. The kernel picks a role by putting it in x0 before
//! `eret`-ing to EL0, which is the closest thing to `argv` that exists at this
//! stage.
//!
//! Nothing here can do anything except through `svc #0`: no ambient authority,
//! no shared memory with the kernel, and no capability it was not handed.

#![no_std]
#![no_main]

mod sys;

use sys::*;

pub const ROLE_SENDER: usize = 0;
pub const ROLE_RECEIVER: usize = 1;
pub const ROLE_INTRUDER: usize = 2;
pub const ROLE_TRESPASSER: usize = 3;

/// Writable process-private data. Every process maps this at the same virtual
/// address, and each sees only its own copy — which is the whole claim of
/// address-space isolation, testable in two lines.
static mut MARKER: usize = 0;

/// Capability slot the kernel populates before starting us. Slot 0 by
/// convention; the intruder is given nothing, so its slot 0 is empty.
const CAP_CHANNEL: usize = 0;

/// The kernel revokes the root capability at tick 60. Both parties retry after
/// this, without being told anything happened — they find out from the kernel's
/// answer, which is the point.
const RETRY_TICK: usize = 80;

#[no_mangle]
pub extern "C" fn _start(role: usize) -> ! {
    match role {
        ROLE_SENDER => sender(),
        ROLE_RECEIVER => receiver(),
        ROLE_INTRUDER => intruder(),
        ROLE_TRESPASSER => trespasser(),
        _ => write("user: unknown role\n"),
    }
    exit(0)
}

/// Write our pid into a private static and report where it lives. Two
/// processes printing the same address with different values is the proof that
/// the address spaces are genuinely separate.
fn show_marker(tag: &str) {
    let addr = &raw const MARKER as usize;
    let pid = getpid();
    unsafe { MARKER = pid };
    write(tag);
    write(" pid ");
    write_dec(pid);
    wrote(" wrote its pid to ", addr);
    write(", reads back ");
    write_dec(unsafe { core::ptr::read_volatile(&raw const MARKER) });
    write("\n");
}

fn wrote(label: &str, addr: usize) {
    write(label);
    write_hex(addr);
}

fn sender() {
    show_marker("  [sender  ]");
    write("  [sender  ] sending on my SEND capability\n");
    match send(CAP_CHANNEL, b"hello from the sender") {
        Ok(_) => write("  [sender  ] send ok\n"),
        Err(e) => { write("  [sender  ] send failed: "); write(errname(e)); write("\n") }
    }

    // We hold SEND, not RECV. The kernel should refuse on rights alone.
    write("  [sender  ] now trying to RECEIVE on the same capability\n");
    let mut buf = [0u8; 64];
    match recv(CAP_CHANNEL, &mut buf) {
        Ok(_) => write("  [sender  ] BUG: receive succeeded without the right\n"),
        Err(e) => { write("  [sender  ] refused, as it should be: "); write(errname(e)); write("\n") }
    }

    // Same capability, same slot, nothing changed on our side.
    wait_until(RETRY_TICK);
    write("  [sender  ] retrying the send with the same capability\n");
    match send(CAP_CHANNEL, b"second message") {
        Ok(_) => write("  [sender  ] BUG: send succeeded after revocation\n"),
        Err(e) => { write("  [sender  ] dead: "); write(errname(e)); write("\n") }
    }
}

fn receiver() {
    show_marker("  [receiver]");
    write("  [receiver] blocking on my RECV capability\n");
    let mut buf = [0u8; 64];
    match recv(CAP_CHANNEL, &mut buf) {
        Ok(n) => {
            write("  [receiver] got: \"");
            write(core::str::from_utf8(&buf[..n]).unwrap_or("<invalid utf8>"));
            write("\"\n");
        }
        Err(e) => { write("  [receiver] receive failed: "); write(errname(e)); write("\n") }
    }

    wait_until(RETRY_TICK);
    write("  [receiver] retrying the receive with the same capability\n");
    match recv(CAP_CHANNEL, &mut buf) {
        Ok(_) => write("  [receiver] BUG: receive succeeded after revocation\n"),
        Err(e) => { write("  [receiver] dead: "); write(errname(e)); write("\n") }
    }
}

fn intruder() {
    // Same channel, same slot number, no capability. Guessing an index is not
    // authority: there is nothing in the slot to name the channel with.
    write("  [intruder] holding no capability; trying slot 0 anyway\n");
    match send(CAP_CHANNEL, b"you should never see this") {
        Ok(_) => write("  [intruder] BUG: send succeeded with no capability\n"),
        Err(e) => { write("  [intruder] denied: "); write(errname(e)); write("\n") }
    }
}

/// Reach for the kernel. EL0 has no business at a higher-half address, and the
/// page tables say so: this faults, the kernel kills this process, and every
/// other process carries on.
fn trespasser() {
    write("  [trespass] reading a kernel address from EL0\n");
    let kernel_va = 0xFFFF_0000_4008_0000usize as *const u64;
    let v = unsafe { core::ptr::read_volatile(kernel_va) };
    write("  [trespass] BUG: still alive, read ");
    write_hex(v as usize);
    write("\n");
}

fn errname(e: isize) -> &'static str {
    match e {
        -1 => "EBADCAP (no such capability)",
        -2 => "EPERM (capability lacks the right)",
        -3 => "EREVOKED (capability was revoked)",
        -4 => "EFAULT (bad user pointer)",
        -5 => "EINVAL",
        _ => "unknown error",
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    write("user: panic\n");
    exit(255)
}
