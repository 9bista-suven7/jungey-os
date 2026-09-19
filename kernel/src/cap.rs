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
    /// The right to invoke a published operation. What an agent is given, and
    /// the thing a delegation chain usually ends at.
    Operation(&'static str),
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
    /// Who this was derived *for*, and *why*.
    ///
    /// Rights say what a capability permits. Provenance says how it came to
    /// exist, and that is the question anyone actually asks afterwards: not
    /// "could the assistant delete that file" but "on whose authority, and for
    /// what". Without it a capability tree is a set of permissions with no
    /// account of how they were granted.
    pub holder: &'static str,
    pub purpose: &'static str,
}

/// One entry per capability ever minted: who it was derived from, who held it,
/// and why. Kept outside the capability tables so a chain stays walkable after
/// the capabilities themselves are gone — which is exactly when someone asks.
#[derive(Clone, Copy)]
pub struct Provenance {
    pub parent: u64,
    pub holder: &'static str,
    pub purpose: &'static str,
    pub rights: u32,
}

static LEDGER: SpinLock<Vec<Provenance>> = SpinLock::new(Vec::new());

fn register(parent: u64, holder: &'static str, purpose: &'static str, rights: u32) -> u64 {
    let mut l = LEDGER.lock();
    l.push(Provenance { parent, holder, purpose, rights });
    l.len() as u64
}

/// The chain of delegation that produced `id`, nearest first.
///
/// This is the answer to "what did the assistant do last Tuesday, and on whose
/// authority": every link says who held the capability and what it was for,
/// back to whoever first created the object.
pub fn chain(id: u64, out: &mut [(u64, Provenance)]) -> usize {
    let ledger = LEDGER.lock();
    let mut cur = id;
    let mut n = 0;
    while cur != 0 && n < out.len() {
        let Some(&p) = ledger.get(cur as usize - 1) else { break };
        out[n] = (cur, p);
        n += 1;
        cur = p.parent;
    }
    n
}

impl Cap {
    /// Mint the first capability to a newly created object.
    pub fn root(obj: Obj, rights: u32) -> Cap {
        Cap::root_for(obj, rights, "kernel", "created the object")
    }

    /// Mint a root capability, recording who it is for and why.
    pub fn root_for(obj: Obj, rights: u32, holder: &'static str, purpose: &'static str) -> Cap {
        Cap {
            id: register(0, holder, purpose, rights),
            obj,
            rights,
            revoked: false,
            holder,
            purpose,
        }
    }

    /// Derive a weaker capability. Rights are intersected, never unioned: this
    /// is the only way to make a new capability, so authority can only shrink
    /// as it is delegated.
    pub fn derive(&self, rights: u32) -> Cap {
        self.derive_for(rights, "unnamed", "unstated")
    }

    /// Derive, saying who it is for and what it is for. Every delegation an
    /// agent makes goes through here, so the chain is complete by construction
    /// rather than by remembering to log it.
    pub fn derive_for(&self, rights: u32, holder: &'static str, purpose: &'static str) -> Cap {
        let rights = self.rights & rights;
        Cap {
            id: register(self.id, holder, purpose, rights),
            obj: self.obj,
            rights,
            revoked: self.revoked,
            holder,
            purpose,
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
        rights_name(self.rights)
    }

    /// What kind of thing this points at, for the audit output.
    pub fn obj_str(&self) -> &'static str {
        match self.obj {
            Obj::Channel(_) => "channel",
            Obj::Mmio { .. } => "mmio",
            Obj::Irq(_) => "irq",
            Obj::Dma { .. } => "dma",
            Obj::Operation(_) => "operation",
        }
    }
}

/// Name a set of rights, for reports.
///
/// Its own function rather than a method, because the interesting place to
/// print rights is a delegation chain — where the capabilities themselves are
/// often gone and only the ledger's record of them remains.
pub fn rights_name(rights: u32) -> &'static str {
    match rights & RIGHTS_ALL {
        r if r == RIGHTS_ALL => "all",
        r if r == RIGHT_SEND | RIGHT_RECV | RIGHT_GRANT => "send+recv+grant",
        r if r == RIGHT_SEND | RIGHT_RECV => "send+recv",
        RIGHT_SEND => "send",
        RIGHT_RECV => "recv",
        RIGHT_MAP => "map",
        RIGHT_IRQ => "irq",
        RIGHT_GRANT => "grant",
        0 => "none",
        _ => "mixed",
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
            Some(p) => p.parent,
            None => return false,
        };
    }
    false
}

/// Total capabilities ever minted, derived ones included.
pub fn minted() -> usize {
    LEDGER.lock().len()
}
