//! The system call interface.
//!
//! Seven calls. Everything a process can do goes through here, and every one
//! that touches an object resolves a capability first — there is no call that
//! takes a global name.

use crate::cap::{Obj, RIGHT_IRQ, RIGHT_MAP, RIGHT_RECV, RIGHT_SEND};
use crate::mm::paging::Perm;
use crate::mm::PAGE_SIZE;
use crate::exceptions::TrapFrame;
use crate::mm::uaccess;
use crate::{gic, ipc, irq, model, proc, sched, time};

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_YIELD: u64 = 2;
pub const SYS_SEND: u64 = 3;
pub const SYS_RECV: u64 = 4;
pub const SYS_GETPID: u64 = 5;
pub const SYS_TICKS: u64 = 6;
pub const SYS_SLEEP: u64 = 7;
pub const SYS_MAP_DEVICE: u64 = 8;
pub const SYS_DMA_MAP: u64 = 9;
pub const SYS_IRQ_WAIT: u64 = 10;
pub const SYS_MODEL_OPEN: u64 = 11;
pub const SYS_MODEL_MAP: u64 = 12;
pub const SYS_MODEL_INFO: u64 = 13;

pub const EBADCAP: isize = -1;
pub const EPERM: isize = -2;
pub const EREVOKED: isize = -3;
pub const EFAULT: isize = -4;
pub const EINVAL: isize = -5;
pub const ETIMEDOUT: isize = -6;

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
        SYS_MAP_DEVICE => sys_map_device(pid, a, b),
        SYS_DMA_MAP => sys_dma_map(pid, a, b),
        SYS_IRQ_WAIT => sys_irq_wait(pid, a, b as u64),
        SYS_MODEL_OPEN => sys_model_open(pid, a, b),
        SYS_MODEL_MAP => sys_model_map(pid, a as u64, b),
        SYS_MODEL_INFO => sys_model_info(pid, a as u64, b),
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
    let Some(channel) = cap.channel() else {
        return EINVAL;
    };
    let Some(Ok(bytes)) = with_space(pid, |s| uaccess::copy_from_user(s, ptr, len)) else {
        return EFAULT;
    };
    match ipc::send(channel, pid, bytes) {
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

        let Some(channel) = cap.channel() else {
            return EINVAL;
        };
        // Take a message, or become a waiter, with no gap between the two: see
        // `ipc::recv_or_prepare`. Checking the queue and then marking oneself
        // blocked leaves a window in which a sender wakes a thread that is not
        // yet waiting, and the message sits unread until someone sends another.
        if let Some(msg) = ipc::recv_or_prepare(channel, channel as u64) {
            let n = msg.bytes.len().min(len);
            let Some(Ok(())) = with_space(pid, |s| uaccess::copy_to_user(s, ptr, &msg.bytes[..n]))
            else {
                return EFAULT;
            };
            return n as isize;
        }

        sched::block(channel as u64);
    }
}

/// Resolve a slot to a capability that carries `right`, or the reason it did not.
fn resolve(pid: usize, slot: usize, right: u32) -> Result<crate::cap::Cap, isize> {
    let cap = proc::lookup_cap(pid, slot).ok_or(EBADCAP)?;
    if cap.revoked {
        return Err(EREVOKED);
    }
    if !cap.allows(right) {
        return Err(EPERM);
    }
    Ok(cap)
}

/// Map a device's registers into the caller's address space. Returns the byte
/// offset of the registers within the mapping.
///
/// The window comes from the capability, not from the argument: a driver says
/// *where in its own address space* it wants the registers, never *which*
/// registers. There is no argument it could pass to reach a device it was not
/// given.
///
/// **Sub-page windows are not isolated, and cannot be.** virtio-mmio puts its
/// transports 0x200 apart, so eight of them share a 4 KiB page, and an MMU that
/// maps pages cannot give a driver one without the other seven. The offset is
/// returned rather than hidden precisely so this is visible: a driver holding
/// one of these capabilities can reach its neighbours' registers, and the only
/// real fixes are hardware that spaces its devices a page apart, an SMMU, or a
/// trusted shim. Linux and VFIO hit the same wall and make the same compromise.
fn sys_map_device(pid: usize, slot: usize, uva: usize) -> isize {
    let cap = match resolve(pid, slot, RIGHT_MAP) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let Obj::Mmio { base, size } = cap.obj else {
        return EINVAL;
    };
    let Some(p) = proc::get(pid) else { return EINVAL };

    let aligned = crate::mm::page_align_down(base);
    let offset = base - aligned;
    let pages = (offset + size).div_ceil(PAGE_SIZE);
    match unsafe { (*p).space.map(uva, aligned, pages, Perm::UserDevice) } {
        Ok(()) => offset as isize,
        Err(_) => EINVAL,
    }
}

/// Map a DMA region and tell the caller its physical address.
///
/// A device is pointed at physical memory, so a driver has to know one. This is
/// the only place the kernel discloses a physical address, and only for memory
/// the caller already holds a capability to.
fn sys_dma_map(pid: usize, slot: usize, uva: usize) -> isize {
    let cap = match resolve(pid, slot, RIGHT_MAP) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let Obj::Dma { base, pages } = cap.obj else {
        return EINVAL;
    };
    let Some(p) = proc::get(pid) else { return EINVAL };
    match unsafe { (*p).space.map(uva, base, pages, Perm::UserData) } {
        Ok(()) => base as isize,
        Err(_) => EINVAL,
    }
}

/// Wait for the caller's interrupt to fire, up to `timeout` ticks.
///
/// Returns how many times it has fired in total. The timeout is not optional:
/// a driver that can block forever on a line that never asserts is a driver
/// that can wedge itself, and the kernel should not offer a way to do that by
/// accident.
fn sys_irq_wait(pid: usize, slot: usize, timeout: u64) -> isize {
    let cap = match resolve(pid, slot, RIGHT_IRQ) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let Obj::Irq(intid) = cap.obj else {
        return EINVAL;
    };
    let Some(p) = proc::get(pid) else { return EINVAL };

    // Re-arm: the line was masked when it last fired, because only the driver
    // can quiet the device.
    gic::enable_spi(intid);

    let deadline = time::ticks() + timeout;
    loop {
        let seq = irq::sequence(intid).unwrap_or(0);
        let seen = unsafe { (*p).irq_seen };
        if seq > seen {
            unsafe { (*p).irq_seen = seq };
            return seq as isize;
        }
        if time::ticks() >= deadline {
            return ETIMEDOUT;
        }
        sched::yield_now();
    }
}

/// Open a model by name. Returns its id.
fn sys_model_open(pid: usize, ptr: usize, len: usize) -> isize {
    if len > 64 {
        return EINVAL;
    }
    let Some(Ok(bytes)) = with_space(pid, |s| uaccess::copy_from_user(s, ptr, len)) else {
        return EFAULT;
    };
    let Ok(name) = core::str::from_utf8(&bytes) else {
        return EINVAL;
    };
    match model::open_by_name(name) {
        Ok(id) => id as isize,
        Err(e) => {
            crate::println!("  model      : open of \"{}\" failed — {}", name, e);
            EBADCAP
        }
    }
}

/// Map a model at `uva`. Returns its size in bytes.
///
/// Nothing is resident afterwards. The pages arrive as the process touches
/// them, which is what makes a multi-gigabyte model start in milliseconds
/// instead of seconds.
fn sys_model_map(pid: usize, id: u64, uva: usize) -> isize {
    if uva % PAGE_SIZE != 0 {
        return EINVAL;
    }
    match model::attach(id, pid, uva) {
        Ok(size) => {
            proc::add_vma(
                pid,
                proc::Vma {
                    start: uva,
                    pages: size.div_ceil(PAGE_SIZE),
                    kind: proc::VmaKind::Model { id },
                },
            );
            size as isize
        }
        Err(_) => EBADCAP,
    }
}

/// Write eight u64s about a model to `out`: id, hash, size, resident pages,
/// total pages, faults, pages reclaimed, reference count.
fn sys_model_info(pid: usize, id: u64, out: usize) -> isize {
    let Some(st) = model::stat(id) else {
        return EBADCAP;
    };
    let vals = [
        st.0,
        st.1,
        st.2 as u64,
        st.3 as u64,
        st.4 as u64,
        st.5,
        st.6,
        st.7 as u64,
    ];
    let mut bytes = [0u8; 64];
    for (i, v) in vals.iter().enumerate() {
        bytes[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    match with_space(pid, |s| uaccess::copy_to_user(s, out, &bytes)) {
        Some(Ok(())) => 0,
        _ => EFAULT,
    }
}
