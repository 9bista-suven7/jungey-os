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

mod blkdrv;
mod sys;

use sys::*;

pub const ROLE_SENDER: usize = 0;
pub const ROLE_RECEIVER: usize = 1;
pub const ROLE_INTRUDER: usize = 2;
pub const ROLE_TRESPASSER: usize = 3;
pub const ROLE_BLKDRV: usize = 4;

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
        ROLE_BLKDRV => blkdrv::run(),
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
    Line::new()
        .s(tag)
        .s(" pid ")
        .d(pid)
        .s(" wrote its pid to ")
        .x(addr)
        .s(", reads back ")
        .d(unsafe { core::ptr::read_volatile(&raw const MARKER) })
        .nl();
}

/// One line, one syscall: `tag`, then `text`.
pub fn say(tag: &str, text: &str) {
    Line::new().s(tag).s(text).nl();
}

fn say_err(tag: &str, text: &str, e: isize) {
    Line::new().s(tag).s(text).s(errname(e)).nl();
}

const SENDER: &str = "  [sender  ]";

fn sender() {
    show_marker(SENDER);
    say(SENDER, " sending on my SEND capability");
    match send(CAP_CHANNEL, b"hello from the sender") {
        Ok(_) => say(SENDER, " send ok"),
        Err(e) => say_err(SENDER, " send failed: ", e),
    }

    // We hold SEND, not RECV. The kernel should refuse on rights alone.
    say(SENDER, " now trying to RECEIVE on the same capability");
    let mut buf = [0u8; 64];
    match recv(CAP_CHANNEL, &mut buf) {
        Ok(_) => say(SENDER, " BUG: receive succeeded without the right"),
        Err(e) => say_err(SENDER, " refused, as it should be: ", e),
    }

    // Same capability, same slot, nothing changed on our side.
    wait_until(RETRY_TICK);
    say(SENDER, " retrying the send with the same capability");
    match send(CAP_CHANNEL, b"second message") {
        Ok(_) => say(SENDER, " BUG: send succeeded after revocation"),
        Err(e) => say_err(SENDER, " dead: ", e),
    }
}

const RECEIVER: &str = "  [receiver]";

fn receiver() {
    show_marker(RECEIVER);
    say(RECEIVER, " blocking on my RECV capability");
    let mut buf = [0u8; 64];
    match recv(CAP_CHANNEL, &mut buf) {
        Ok(n) => {
            Line::new()
                .s(RECEIVER)
                .s(" got: \"")
                .s(core::str::from_utf8(&buf[..n]).unwrap_or("<invalid utf8>"))
                .s("\"")
                .nl();
        }
        Err(e) => say_err(RECEIVER, " receive failed: ", e),
    }

    wait_until(RETRY_TICK);
    say(RECEIVER, " retrying the receive with the same capability");
    match recv(CAP_CHANNEL, &mut buf) {
        Ok(_) => say(RECEIVER, " BUG: receive succeeded after revocation"),
        Err(e) => say_err(RECEIVER, " dead: ", e),
    }
}

const INTRUDER: &str = "  [intruder]";

fn intruder() {
    // Same channel, same slot number, no capability. Guessing an index is not
    // authority: there is nothing in the slot to name the channel with.
    say(INTRUDER, " holding no capability; trying slot 0 anyway");
    match send(CAP_CHANNEL, b"you should never see this") {
        Ok(_) => say(INTRUDER, " BUG: send succeeded with no capability"),
        Err(e) => say_err(INTRUDER, " denied: ", e),
    }
}

/// Reach for the kernel. EL0 has no business at a higher-half address, and the
/// page tables say so: this faults, the kernel kills this process, and every
/// other process carries on.
fn trespasser() {
    say("  [trespass]", " reading a kernel address from EL0");
    let kernel_va = 0xFFFF_0000_4008_0000usize as *const u64;
    let v = unsafe { core::ptr::read_volatile(kernel_va) };
    Line::new().s("  [trespass] BUG: still alive, read ").x(v as usize).nl();
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
