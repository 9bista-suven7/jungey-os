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
mod font;
mod gpudrv;
mod sys;

use sys::*;

pub const ROLE_SENDER: usize = 0;
pub const ROLE_RECEIVER: usize = 1;
pub const ROLE_INTRUDER: usize = 2;
pub const ROLE_TRESPASSER: usize = 3;
pub const ROLE_BLKDRV: usize = 4;
pub const ROLE_MODEL_A: usize = 5;
pub const ROLE_MODEL_B: usize = 6;
pub const ROLE_INFER_UI: usize = 7;
pub const ROLE_INFER_BG: usize = 8;
pub const ROLE_GPUDRV: usize = 9;

/// Where a mapped model goes in our address space. High enough to be clear of
/// the image and the heap, low enough to be obviously user memory.
const MODEL_VA: usize = 0x2000_0000;

/// The pattern the kernel wrote into the model file. Checking it on every page
/// is what turns "the mapping worked" into "the right bytes arrived".
fn model_byte(offset: usize) -> u8 {
    (offset.wrapping_mul(31) ^ (offset >> 12).wrapping_mul(17)) as u8
}

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
        ROLE_MODEL_A => model_user("  [modelA  ]"),
        ROLE_MODEL_B => model_user("  [modelB  ]"),
        ROLE_INFER_UI => infer_interactive(),
        ROLE_INFER_BG => infer_background(),
        ROLE_GPUDRV => gpudrv::run(),
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

/// Touch every page of the mapping and check what arrived.
///
/// The first pass faults each page in. The second runs after the kernel has
/// reclaimed some of them, so it re-faults — and the bytes must still be right,
/// because a weight page that is dropped and re-read is supposed to be
/// indistinguishable from one that was never dropped.
fn sweep(pages: usize) -> (usize, usize) {
    let mut checked = 0;
    let mut wrong = 0;
    for p in 0..pages {
        for k in 0..8 {
            let off = p * 4096 + k * 512;
            let got = unsafe { core::ptr::read_volatile((MODEL_VA + off) as *const u8) };
            if got != model_byte(off) {
                wrong += 1;
            }
            checked += 1;
        }
    }
    (checked, wrong)
}

fn model_user(tag: &str) {
    let id = model_open("model.bin");
    if id < 0 {
        say(tag, " cannot open model.bin");
        return;
    }
    let size = model_map(id as usize, MODEL_VA);
    if size < 0 {
        say(tag, " cannot map the model");
        return;
    }
    let pages = (size as usize + 4095) / 4096;

    let mut info = [0u64; 8];
    let _ = model_info(id as usize, &mut info);
    Line::new()
        .s(tag)
        .s(" mapped model ")
        .d(id as usize)
        .s(" (")
        .d(size as usize)
        .s(" bytes, ")
        .d(pages)
        .s(" pages), resident ")
        .d(info[3] as usize)
        .nl();

    let (checked, wrong) = sweep(pages);
    let _ = model_info(id as usize, &mut info);
    Line::new()
        .s(tag)
        .s(" pass 1: ")
        .d(checked)
        .s(" bytes checked, ")
        .d(wrong)
        .s(" wrong, ")
        .d(info[3] as usize)
        .s(" of ")
        .d(info[4] as usize)
        .s(" pages resident, ")
        .d(info[5] as usize)
        .s(" faults")
        .nl();

    // Wait for the kernel to reclaim under simulated pressure, then touch the
    // same pages again. Watching the model's own counters rather than sleeping
    // a fixed time is what makes this deterministic: the second pass starts
    // when there is something to re-fault, not when a timer says so.
    let mut waited = 0;
    while info[6] == 0 && waited < 400 {
        sleep(2);
        waited += 2;
        let _ = model_info(id as usize, &mut info);
    }

    let (checked2, wrong2) = sweep(pages);
    let _ = model_info(id as usize, &mut info);
    Line::new()
        .s(tag)
        .s(" pass 2: ")
        .d(checked2)
        .s(" bytes checked, ")
        .d(wrong2)
        .s(" wrong after ")
        .d(info[6] as usize)
        .s(" pages were reclaimed")
        .nl();
}

/// Report a finished job the way an application would see it.
fn report_job(tag: &str, id: usize) {
    let mut st = [0u64; 11];
    if tensor_stat(id, &mut st) != 0 {
        say(tag, " no such job");
        return;
    }
    let mut l = Line::new();
    l.s(tag)
        .s(" job ")
        .d(st[0] as usize)
        .s(": ")
        .d(st[3] as usize)
        .s("/")
        .d(st[2] as usize)
        .s(" segments, latency ")
        .d((st[4] / 1000) as usize)
        .s(" ms");
    if st[5] != 0 {
        l.s(", deadline ").s(if st[6] == 1 { "MET" } else { "MISSED" });
    }
    l.s(", preempted ")
        .d(st[7] as usize)
        .s(" times, ")
        .d((st[8] / 1000) as usize)
        .s(" mJ")
        .nl();
}

/// A long, patient job. Nobody is waiting on it, and it should be the one that
/// gives way.
fn infer_background() {
    const TAG: &str = "  [infer-bg]";
    say(TAG, " submitting 150 background segments, no deadline");
    let id = tensor_submit(QOS_BACKGROUND, 150, 0);
    if id < 0 {
        say(TAG, " refused");
        return;
    }
    if tensor_wait(id as usize, 3000) != 1 {
        say(TAG, " did not finish in time");
        return;
    }
    report_job(TAG, id as usize);
}

/// The user is waiting. Arrives after the background job is already running,
/// and has 50 ms to be done.
fn infer_interactive() {
    const TAG: &str = "  [infer-ui]";

    // Let the background job get hold of the device first, so this genuinely
    // has to take it away rather than just finding it free.
    sleep(5);

    say(TAG, " submitting 5 interactive segments with a 50 ms deadline");
    let id = tensor_submit(QOS_INTERACTIVE, 5, 50_000);
    if id < 0 {
        say(TAG, " refused — the deadline could not be met");
        return;
    }
    if tensor_wait(id as usize, 500) != 1 {
        say(TAG, " did not finish in time");
        return;
    }
    report_job(TAG, id as usize);

    // Now ask for something that cannot be delivered. A system that accepts
    // this and misses is worse than one that says no.
    say(TAG, " now asking for 400 segments within 20 ms");
    let bad = tensor_submit(QOS_INTERACTIVE, 400, 20_000);
    if bad < 0 {
        say(TAG, " refused, as it should be — the app can pick a smaller model");
    } else {
        say(TAG, " BUG: accepted work it cannot finish in time");
    }
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
