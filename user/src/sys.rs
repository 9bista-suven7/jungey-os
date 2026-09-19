//! Syscall stubs. The entire interface between a process and the kernel.

pub const SYS_EXIT: usize = 0;
pub const SYS_WRITE: usize = 1;
pub const SYS_YIELD: usize = 2;
pub const SYS_SEND: usize = 3;
pub const SYS_RECV: usize = 4;
pub const SYS_GETPID: usize = 5;
pub const SYS_TICKS: usize = 6;
pub const SYS_SLEEP: usize = 7;

#[inline(always)]
unsafe fn syscall3(n: usize, a: usize, b: usize, c: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a => ret,
        in("x1") b,
        in("x2") c,
        options(nostack),
    );
    ret
}

pub fn write(s: &str) {
    unsafe { syscall3(SYS_WRITE, s.as_ptr() as usize, s.len(), 0) };
}

pub fn exit(code: usize) -> ! {
    unsafe { syscall3(SYS_EXIT, code, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

pub fn yield_now() {
    unsafe { syscall3(SYS_YIELD, 0, 0, 0) };
}

pub fn getpid() -> usize {
    unsafe { syscall3(SYS_GETPID, 0, 0, 0) as usize }
}

/// Scheduler ticks since boot. The only clock a process gets for now.
pub fn ticks() -> usize {
    unsafe { syscall3(SYS_TICKS, 0, 0, 0) as usize }
}

/// Leave the run queue for `n` ticks. Unlike a yield loop this takes the
/// process off the run queue entirely, so the core can actually idle.
pub fn sleep(n: usize) {
    unsafe { syscall3(SYS_SLEEP, n, 0, 0) };
}

/// Sleep until the kernel's tick counter reaches `t`.
pub fn wait_until(t: usize) {
    let now = ticks();
    if t > now {
        sleep(t - now);
    }
}

/// Minimal unsigned decimal output: there is no formatter down here.
pub fn write_dec(mut n: usize) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    write(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
}

pub fn write_hex(n: usize) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        buf[2 + i] = DIGITS[(n >> (60 - i * 4)) & 0xf];
    }
    write(unsafe { core::str::from_utf8_unchecked(&buf) });
}

pub fn send(cap: usize, msg: &[u8]) -> Result<usize, isize> {
    let r = unsafe { syscall3(SYS_SEND, cap, msg.as_ptr() as usize, msg.len()) };
    if r < 0 { Err(r) } else { Ok(r as usize) }
}

pub fn recv(cap: usize, buf: &mut [u8]) -> Result<usize, isize> {
    let r = unsafe { syscall3(SYS_RECV, cap, buf.as_mut_ptr() as usize, buf.len()) };
    if r < 0 { Err(r) } else { Ok(r as usize) }
}
