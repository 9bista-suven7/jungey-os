//! Capabilities.
//!
//! The one idea the rest of this OS rests on: a process's authority is exactly
//! the set of capabilities in its table, and nothing else. There is no ambient
//! permission to check, no global name a process can guess its way to, and no
//! way to widen a capability you were handed — only to narrow it.
//!
//! Each capability records the one it was derived from, so authority forms a
//! tree rooted at whoever created the object. Revoking a node kills the whole
//! subtree in one operation, which is what makes "revoke everything that came
//! from the consent I gave that app" a single call rather than an audit.
//!
//! Stage 6's delegation chains (user -> agent -> tool -> resource) are this
//! structure with provenance recorded at each edge.

use crate::sync::SpinLock;
use alloc::vec::Vec;

pub const RIGHT_SEND: u32 = 1 << 0;
pub const RIGHT_RECV: u32 = 1 << 1;
/// May hand derived copies to another process. Not exercised yet; present so
/// derivation cannot quietly become universal later.
pub const RIGHT_GRANT: u32 = 1 << 2;

/// May map a device's registers, or a DMA region, into its address space.
pub const RIGHT_MAP: u32 = 1 << 3;
/// May wait on an interrupt.
pub const RIGHT_IRQ: u32 = 1 << 4;

pub const RIGHTS_ALL: u32 = RIGHT_SEND | RIGHT_RECV | RIGHT_GRANT | RIGHT_MAP | RIGHT_IRQ;

/// What a capability points at.
///
/// Hardware is named the same way everything else is. A driver is an ordinary
/// process; what makes it a driver is holding an `Mmio` capability to one
/// device's registers, an `Irq` to that device's line, and a `Dma` region the
/// device can reach. It cannot touch a second device, and revoking its
/// capabilities stops it as surely as killing it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Obj {
    Channel(usize),
    /// A device register window, by physical address.
    Mmio { base: usize, size: usize },
    /// A GIC interrupt.
    Irq(u32),
    /// Physically contiguous memory a device can be pointed at.
    Dma { base: usize, pages: usize },
}

#[derive(Clone, Copy)]
pub struct Cap {
    /// Unique and never reused, so a stale id can be recognised rather than
    /// resolving to whatever took its place.
    pub id: u64,
    pub obj: Obj,
    pub rights: u32,
    /// Tombstoned rather than removed: a revoked capability must be
    /// distinguishable from one that was never granted.
    pub revoked: bool,
}

/// `parent[id - 1]` is the capability `id` was derived from; 0 means root.
/// Kept outside the capability tables so an ancestor chain stays walkable after
/// the ancestors themselves are gone.
static LEDGER: SpinLock<Vec<u64>> = SpinLock::new(Vec::new());

fn register(parent: u64) -> u64 {
    let mut l = LEDGER.lock();
    l.push(parent);
    l.len() as u64
}

impl Cap {
    /// Mint the first capability to a newly created object.
    pub fn root(obj: Obj, rights: u32) -> Cap {
        Cap { id: register(0), obj, rights, revoked: false }
    }

    /// Derive a weaker capability. Rights are intersected, never unioned: this
    /// is the only way to make a new capability, so authority can only shrink
    /// as it is delegated.
    pub fn derive(&self, rights: u32) -> Cap {
        Cap {
            id: register(self.id),
            obj: self.obj,
            rights: self.rights & rights,
            revoked: self.revoked,
        }
    }

    pub fn allows(&self, right: u32) -> bool {
        !self.revoked && self.rights & right != 0
    }

    pub fn channel(&self) -> Option<usize> {
        match self.obj {
            Obj::Channel(c) => Some(c),
            _ => None,
        }
    }

    /// Human-readable rights, for the audit output.
    pub fn rights_str(&self) -> &'static str {
        match self.rights & (RIGHT_SEND | RIGHT_RECV | RIGHT_MAP | RIGHT_IRQ) {
            r if r == RIGHT_SEND | RIGHT_RECV => "send+recv",
            RIGHT_SEND => "send",
            RIGHT_RECV => "recv",
            RIGHT_MAP => "map",
            RIGHT_IRQ => "irq",
            _ => "none",
        }
    }

    /// What kind of thing this points at, for the audit output.
    pub fn obj_str(&self) -> &'static str {
        match self.obj {
            Obj::Channel(_) => "channel",
            Obj::Mmio { .. } => "mmio",
            Obj::Irq(_) => "irq",
            Obj::Dma { .. } => "dma",
        }
    }
}

/// Is `id` `ancestor`, or descended from it?
pub fn is_descendant(id: u64, ancestor: u64) -> bool {
    let ledger = LEDGER.lock();
    let mut cur = id;
    while cur != 0 {
        if cur == ancestor {
            return true;
        }
        cur = match ledger.get(cur as usize - 1) {
            Some(&p) => p,
            None => return false,
        };
    }
    false
}

/// Total capabilities ever minted, derived ones included.
pub fn minted() -> usize {
    LEDGER.lock().len()
}
