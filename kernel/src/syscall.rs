//! The system call interface.
//!
//! Seven calls. Everything a process can do goes through here, and every one
//! that touches an object resolves a capability first — there is no call that
//! takes a global name.

use crate::cap::{RIGHT_RECV, RIGHT_SEND};
use crate::exceptions::TrapFrame;
use crate::mm::uaccess;
use crate::{ipc, proc, sched, time};

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_YIELD: u64 = 2;
pub const SYS_SEND: u64 = 3;
pub const SYS_RECV: u64 = 4;
pub const SYS_GETPID: u64 = 5;
pub const SYS_TICKS: u64 = 6;
pub const SYS_SLEEP: u64 = 7;

pub const EBADCAP: isize = -1;
pub const EPERM: isize = -2;
pub const EREVOKED: isize = -3;
pub const EFAULT: isize = -4;
pub const EINVAL: isize = -5;

/// Called from the synchronous-exception path when a lower EL executes SVC.
pub fn dispatch(frame: &mut TrapFrame) {
    let n = frame.x[8];
    let (a, b, c) = (frame.x[0] as usize, frame.x[1] as usize, frame.x[2] as usize);

    let Some(pid) = sched::current_pid() else {
        frame.x[0] = EINVAL as u64;
        return;
    };

    let ret: isize = match n {
        SYS_EXIT => {
            if let Some(p) = proc::get(pid) {
                unsafe { (*p).exit_code = Some(a as isize) };
            }
            sched::thread_exit(); // does not return
        }
        SYS_WRITE => sys_write(pid, a, b),
        SYS_YIELD => {
            sched::yield_now();
            0
        }
        SYS_SEND => sys_send(pid, a, b, c),
        SYS_RECV => sys_recv(pid, a, b, c),
        SYS_GETPID => pid as isize,
        SYS_TICKS => time::ticks() as isize,
        SYS_SLEEP => {
            sched::sleep_ticks(a as u64);
            0
        }
        _ => EINVAL,
    };

    frame.x[0] = ret as u64;
}

fn with_space<R>(pid: usize, f: impl FnOnce(&crate::mm::paging::AddressSpace) -> R) -> Option<R> {
    let p = proc::get(pid)?;
    Some(f(unsafe { &(*p).space }))
}

fn sys_write(pid: usize, ptr: usize, len: usize) -> isize {
    if len > uaccess::MAX_TRANSFER {
        return EINVAL;
    }
    let Some(Ok(bytes)) = with_space(pid, |s| uaccess::copy_from_user(s, ptr, len)) else {
        return EFAULT;
    };
    match core::str::from_utf8(&bytes) {
        Ok(s) => {
            print!("{}", s);
            len as isize
        }
        Err(_) => EINVAL,
    }
}

fn sys_send(pid: usize, slot: usize, ptr: usize, len: usize) -> isize {
    let Some(cap) = proc::lookup_cap(pid, slot) else {
        return EBADCAP;
    };
    if cap.revoked {
        return EREVOKED;
    }
    if !cap.allows(RIGHT_SEND) {
        return EPERM;
    }
    let Some(Ok(bytes)) = with_space(pid, |s| uaccess::copy_from_user(s, ptr, len)) else {
        return EFAULT;
    };
    match ipc::send(cap.channel(), pid, bytes) {
        Ok(n) => n as isize,
        Err(_) => EINVAL,
    }
}

fn sys_recv(pid: usize, slot: usize, ptr: usize, len: usize) -> isize {
    loop {
        // Re-resolve every time round: the capability may have been revoked
        // while this thread was blocked, and a blocked receiver must find out.
        let Some(cap) = proc::lookup_cap(pid, slot) else {
            return EBADCAP;
        };
        if cap.revoked {
            return EREVOKED;
        }
        if !cap.allows(RIGHT_RECV) {
            return EPERM;
        }

        if let Some(msg) = ipc::try_recv(cap.channel()) {
            let n = msg.bytes.len().min(len);
            let Some(Ok(())) = with_space(pid, |s| uaccess::copy_to_user(s, ptr, &msg.bytes[..n]))
            else {
                return EFAULT;
            };
            return n as isize;
        }

        sched::block_on(cap.channel() as u64);
    }
}
