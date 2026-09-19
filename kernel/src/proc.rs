//! Processes: an address space plus a capability table.
//!
//! Deliberately thin. A process is not a bundle of permissions or an identity
//! the kernel reasons about — it is a page table and a list of the capabilities
//! it was handed. Everything it is allowed to do follows from that list.

use crate::cap::{Cap, Obj};
use crate::elf;
use crate::mm::paging::{AddressSpace, Perm};
use crate::mm::PAGE_SIZE;
use crate::sync::SpinLock;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// Capability slots per process. Small on purpose: a process that needs many is
/// usually one that should have been split.
pub const CAP_SLOTS: usize = 8;

/// Top of the user stack. Below the ELF image's 4 MiB load address there is a
/// deliberate unmapped gap; above the stack there is nothing at all.
const USER_STACK_TOP: usize = 0x0000_0000_7000_0000;
const USER_STACK_PAGES: usize = 4;

pub struct Process {
    pub pid: usize,
    pub name: &'static str,
    pub space: AddressSpace,
    pub caps: [Option<Cap>; CAP_SLOTS],
    pub entry: usize,
    pub stack_top: usize,
    /// Handed to the process in x0 at entry. The only argument it ever gets.
    pub arg: usize,
    pub exit_code: Option<isize>,
    /// Interrupts this process has already been told about. One counter,
    /// because a process holds one `Irq` capability at this stage.
    pub irq_seen: u64,
}

/// Processes are leaked for the lifetime of the kernel; reaping arrives with
/// stage 3, along with the refcounted handles that make it safe.
struct ProcTable(Vec<*mut Process>);

// Safety: every dereference happens under the lock, on the one core that runs
// kernel code at this stage.
unsafe impl Send for ProcTable {}

static TABLE: SpinLock<ProcTable> = SpinLock::new(ProcTable(Vec::new()));

/// Build an address space, load `image` into it, and map a stack.
pub fn create(name: &'static str, image: &[u8], arg: usize) -> Result<usize, &'static str> {
    let mut space = AddressSpace::new().ok_or("no memory for an address space")?;
    let entry = elf::load(&mut space, image)?;

    let stack_bottom = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;
    space.map_anonymous(stack_bottom, USER_STACK_PAGES, Perm::UserData)?;

    let mut table = TABLE.lock();
    let pid = table.0.len();
    table.0.push(Box::into_raw(Box::new(Process {
        pid,
        name,
        space,
        caps: [None; CAP_SLOTS],
        entry,
        stack_top: USER_STACK_TOP,
        arg,
        exit_code: None,
        irq_seen: 0,
    })));
    Ok(pid)
}

/// Raw access to a process.
///
/// # Safety
/// One core runs kernel code, and the table only grows, so the pointer stays
/// valid. Stage 3's SMP work replaces this with a refcounted handle.
pub fn get(pid: usize) -> Option<*mut Process> {
    TABLE.lock().0.get(pid).copied()
}

pub fn install_cap(pid: usize, slot: usize, cap: Cap) -> Result<(), &'static str> {
    if slot >= CAP_SLOTS {
        return Err("capability slot out of range");
    }
    let p = get(pid).ok_or("no such process")?;
    unsafe { (*p).caps[slot] = Some(cap) };
    Ok(())
}

/// Look up a slot in the calling process's table.
pub fn lookup_cap(pid: usize, slot: usize) -> Option<Cap> {
    if slot >= CAP_SLOTS {
        return None;
    }
    let p = get(pid)?;
    unsafe { (*p).caps[slot] }
}

/// Revoke `cap_id` and everything derived from it, wherever it ended up.
///
/// This is the operation the whole capability design exists for: one edge cut,
/// the entire subtree of delegated authority dies with it — including in
/// processes that were never told where their capability came from.
pub fn revoke(cap_id: u64) -> usize {
    let mut killed = 0;
    let mut affected_channels: Vec<usize> = Vec::new();

    {
        let table = TABLE.lock();
        for &p in table.0.iter() {
            let proc = unsafe { &mut *p };
            for slot in proc.caps.iter_mut() {
                if let Some(c) = slot {
                    if !c.revoked && crate::cap::is_descendant(c.id, cap_id) {
                        c.revoked = true;
                        killed += 1;
                        if let Obj::Channel(ch) = c.obj {
                            if !affected_channels.contains(&ch) {
                                affected_channels.push(ch);
                            }
                        }
                    }
                }
            }
        }
    }

    // A process blocked in recv on a revoked channel must not wait forever; it
    // wakes and finds its capability dead.
    for ch in affected_channels {
        crate::sched::wake_all_on(ch as u64);
    }
    killed
}

pub fn count() -> usize {
    TABLE.lock().0.len()
}

/// Run `f` over every process, for reporting.
pub fn for_each(mut f: impl FnMut(&Process)) {
    let table = TABLE.lock();
    for &p in table.0.iter() {
        f(unsafe { &*p });
    }
}
