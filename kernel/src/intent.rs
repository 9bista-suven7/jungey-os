//! Typed operations and transactional intents.
//!
//! Two ideas from `docs/ARCHITECTURE.md` sections 3.3 and 3.4, and they only
//! work together.
//!
//! **Applications publish operations, not screens.** An operation says what it
//! is called, what it takes, what it changes, and whether a person should be
//! asked first. An agent composes over that registry. The alternative — which
//! is what agents on phones do today — is to drive the accessibility API and
//! pretend to be a finger, which is fragile, unauditable, and grants the agent
//! everything the user can see.
//!
//! **Every mutating step is recorded before it happens.** An intent groups the
//! steps of a task and keeps what each one is about to overwrite, so the whole
//! thing can be put back. "Undo what it just did" then becomes an operation the
//! system can actually perform rather than an apology.
//!
//! Undo is nearly free here because of a property the filesystem has for
//! unrelated reasons: JLFS never overwrites, so the bytes a file used to
//! contain are still on the disk after it is rewritten. Putting the old
//! directory entry back restores the old contents without copying anything.
//!
//! **What is not here: the planner.** Choosing which operations to compose for
//! a goal stated in English needs a model, and this machine does not run one
//! yet. The `plan` below matches on declared effects, which is enough to show
//! that composition happens over the registry rather than over hardcoded calls,
//! and is not a substitute for the real thing. Everything *around* the planner —
//! the authority it acts under, the record of what it did, the ability to undo
//! it — is what this stage builds, and those are the parts that decide whether
//! an agent is safe to let near anything.

use crate::fs::{FileEntry, Fs};
use crate::sync::SpinLock;
use alloc::string::String;
use alloc::vec::Vec;

/// What an operation does to the world.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Reads. Always safe to retry, never needs undoing.
    ReadOnly,
    /// Changes something the system can put back.
    Mutates,
    /// Leaves the device — sends, posts, pays. Cannot be undone, only
    /// regretted, which is why it is a separate kind rather than a flag.
    External,
}

impl Effect {
    pub fn label(&self) -> &'static str {
        match self {
            Effect::ReadOnly => "read-only",
            Effect::Mutates => "mutates",
            Effect::External => "external",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Operation {
    pub name: &'static str,
    pub provider: &'static str,
    pub args: &'static str,
    pub effect: Effect,
    /// Whether a person should be asked before this runs.
    pub confirm: bool,
}

static REGISTRY: SpinLock<Vec<Operation>> = SpinLock::new(Vec::new());

/// Publish an operation. An application's whole interface to an agent.
pub fn publish(op: Operation) {
    REGISTRY.lock().push(op);
}

pub fn find(name: &str) -> Option<Operation> {
    REGISTRY.lock().iter().find(|o| o.name == name).copied()
}

pub fn for_each_operation(mut f: impl FnMut(&Operation)) {
    for op in REGISTRY.lock().iter() {
        f(op);
    }
}

/// Choose operations for a goal, by the effects it needs.
///
/// A placeholder for a planner, and deliberately a transparent one: it picks
/// the first published operation with each required effect. What it
/// demonstrates is that the agent composes over what applications *declare*,
/// so adding an application adds a capability without anyone rewriting the
/// agent. What it is not is a planner.
pub fn plan(wanted: &[Effect], out: &mut [Operation]) -> usize {
    let reg = REGISTRY.lock();
    let mut n = 0;
    for &effect in wanted {
        if n >= out.len() {
            break;
        }
        if let Some(op) = reg.iter().find(|o| o.effect == effect) {
            out[n] = *op;
            n += 1;
        }
    }
    n
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntentState {
    Open,
    Committed,
    Undone,
}

/// One thing an intent did, and what it needs to put it back.
pub struct Step {
    pub op: &'static str,
    pub target: String,
    /// The directory entry as it was before this step. `None` means the file
    /// did not exist, so undoing means removing it.
    pub before: Option<FileEntry>,
    pub undoable: bool,
}

pub struct Intent {
    pub id: u64,
    pub goal: &'static str,
    pub actor: &'static str,
    /// The capability the whole intent was carried out under, so a reader can
    /// follow it back to whoever authorised it.
    pub cap: u64,
    pub steps: Vec<Step>,
    pub state: IntentState,
}

static INTENTS: SpinLock<Vec<Intent>> = SpinLock::new(Vec::new());
static NEXT_ID: SpinLock<u64> = SpinLock::new(1);

pub fn begin(goal: &'static str, actor: &'static str, cap: u64) -> u64 {
    let mut n = NEXT_ID.lock();
    let id = *n;
    *n += 1;
    drop(n);
    INTENTS.lock().push(Intent {
        id,
        goal,
        actor,
        cap,
        steps: Vec::new(),
        state: IntentState::Open,
    });
    id
}

/// Record a step *before* performing it.
///
/// Before, not after: a step recorded after the fact is a step that cannot be
/// undone if the machine stops in between.
pub fn record(id: u64, op: &'static str, target: &str, before: Option<FileEntry>, undoable: bool) {
    let mut all = INTENTS.lock();
    if let Some(i) = all.iter_mut().find(|i| i.id == id) {
        i.steps.push(Step {
            op,
            target: String::from(target),
            before,
            undoable,
        });
    }
}

pub fn commit(id: u64) {
    let mut all = INTENTS.lock();
    if let Some(i) = all.iter_mut().find(|i| i.id == id) {
        i.state = IntentState::Committed;
    }
}

/// Put everything an intent changed back, newest step first.
///
/// Returns how many steps were reversed and how many could not be. An external
/// step is the second kind: nothing here can un-send a message, and reporting
/// that honestly is more useful than a partial undo that claims success.
pub fn undo(id: u64, fs: &mut Fs) -> Result<(usize, usize), &'static str> {
    // Copy what is needed out from under the lock: putting an entry back is a
    // disk write, which yields.
    let plan: Vec<(&'static str, String, Option<FileEntry>, bool)> = {
        let all = INTENTS.lock();
        let i = all.iter().find(|i| i.id == id).ok_or("no such intent")?;
        if i.state == IntentState::Undone {
            return Err("already undone");
        }
        i.steps
            .iter()
            .rev()
            .map(|s| (s.op, s.target.clone(), s.before, s.undoable))
            .collect()
    };

    let mut reversed = 0;
    let mut refused = 0;
    for (_, target, before, undoable) in plan {
        if !undoable {
            refused += 1;
            continue;
        }
        fs.set_entry(&target, before)?;
        reversed += 1;
    }

    let mut all = INTENTS.lock();
    if let Some(i) = all.iter_mut().find(|i| i.id == id) {
        i.state = IntentState::Undone;
    }
    Ok((reversed, refused))
}

pub fn for_each(mut f: impl FnMut(&Intent)) {
    for i in INTENTS.lock().iter() {
        f(i);
    }
}

pub fn steps(id: u64) -> usize {
    INTENTS
        .lock()
        .iter()
        .find(|i| i.id == id)
        .map(|i| i.steps.len())
        .unwrap_or(0)
}
