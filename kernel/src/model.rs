//! The model store: weights as a kernel object rather than bytes an app read
//! into its heap.
//!
//! This is the second of the five bets in `docs/ARCHITECTURE.md`, and the one
//! that decides whether two models fit on a phone at all. A conventional OS
//! gives every process its own copy of a 3 GB file it `read()` into memory. Here
//! a model is:
//!
//! - **content-addressed**, so two processes asking for the same weights get the
//!   same object and the same physical pages, refcounted once;
//! - **mapped, not loaded**, so a page arrives when something touches it rather
//!   than in a multi-second read before anything can start;
//! - **reclaimable**, because every resident page is clean by construction — it
//!   is a copy of what is on flash, so dropping it costs a re-read and nothing
//!   else. Under pressure these go first, ahead of anything the kernel would
//!   otherwise have to swap.
//!
//! Weight pages are mapped read-only into every process that holds them, which
//! is not a restriction: inference reads weights. Anything that wants to modify
//! them wants a different model.

use crate::blk;
use crate::mm::{frames, paging::Perm, phys_to_virt, PAGE_SIZE};
use crate::sync::SpinLock;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

const SECTORS_PER_PAGE: u64 = (PAGE_SIZE / blk::SECTOR_SIZE) as u64;

/// A process that has this model mapped, and where.
#[derive(Clone, Copy)]
struct Mapping {
    pid: usize,
    base: usize,
}

/// What is behind one page of a model.
///
/// `Loading` matters more than it looks: without it, two cores faulting the
/// same page both read it from flash and one copy is thrown away. That is a
/// wasted read of exactly the kind this module exists to avoid, and on a real
/// model it is a wasted read of a megabyte. The second faulter waits instead.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PageState {
    Absent,
    Loading,
    Resident(usize),
}

pub struct Model {
    pub id: u64,
    /// Content hash. Two files with the same bytes are the same model, however
    /// they were named or whoever asked for them.
    pub hash: u64,
    pub size: usize,
    /// First sector of the backing file, so a fault can read a page without
    /// going back through the filesystem.
    start_sector: u64,
    pages: Vec<PageState>,
    mappings: Vec<Mapping>,
    pub refs: usize,
    pub faults: u64,
    pub reclaimed: u64,
}

impl Model {
    pub fn total_pages(&self) -> usize {
        self.pages.len()
    }
    pub fn resident_pages(&self) -> usize {
        self.pages
            .iter()
            .filter(|p| matches!(p, PageState::Resident(_)))
            .count()
    }
}

struct Store {
    models: Vec<Model>,
    next_id: u64,
}

static STORE: SpinLock<Store> = SpinLock::new(Store { models: Vec::new(), next_id: 1 });

/// Pages currently held by all models, for the memory-pressure report.
static RESIDENT: AtomicU64 = AtomicU64::new(0);
/// Pages served from an already-resident frame — the sharing dividend.
static SHARED_HITS: AtomicU64 = AtomicU64::new(0);

/// FNV-1a. Not cryptographic: this identifies content, it does not authenticate
/// it. Signing a model is a different problem with a different answer.
fn hash_bytes(seed: u64, data: &[u8]) -> u64 {
    let mut h = seed;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// Model files the kernel has published, by name.
///
/// A process names a model, never a sector: it has no way to ask for arbitrary
/// bytes of the disk, and nothing it can pass reaches storage it was not meant
/// to see.
static PUBLISHED: SpinLock<Vec<(&'static str, u64, usize)>> = SpinLock::new(Vec::new());

/// Make a model file openable by name.
pub fn publish(name: &'static str, start_sector: u64, size: usize) {
    let mut p = PUBLISHED.lock();
    if !p.iter().any(|&(n, _, _)| n == name) {
        p.push((name, start_sector, size));
    }
}

/// Open a published model by name.
pub fn open_by_name(name: &str) -> Result<u64, &'static str> {
    let found = PUBLISHED
        .lock()
        .iter()
        .find(|&&(n, _, _)| n == name)
        .copied();
    let Some((n, start, size)) = found else {
        return Err("no such model");
    };
    open(n, start, size)
}

/// Open a model by the name of its backing file.
///
/// Reads the file once to hash it. If those bytes are already in the store —
/// under any name — the existing object is returned and nothing is read again.
pub fn open(name: &str, start_sector: u64, size: usize) -> Result<u64, &'static str> {
    // Hash the content. This is the expensive part of opening a model, and it
    // is what makes the deduplication real rather than a filename comparison.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut sector = [0u8; blk::SECTOR_SIZE];
    let sectors = size.div_ceil(blk::SECTOR_SIZE);
    let mut left = size;
    for s in 0..sectors as u64 {
        blk::read_sector(start_sector + s, &mut sector)?;
        let n = left.min(blk::SECTOR_SIZE);
        hash = hash_bytes(hash, &sector[..n]);
        left -= n;
    }

    let mut store = STORE.lock();
    if let Some(m) = store.models.iter_mut().find(|m| m.hash == hash) {
        m.refs += 1;
        return Ok(m.id);
    }

    let id = store.next_id;
    store.next_id += 1;
    let total = size.div_ceil(PAGE_SIZE);
    store.models.push(Model {
        id,
        hash,
        size,
        start_sector,
        pages: alloc::vec![PageState::Absent; total],
        mappings: Vec::new(),
        refs: 1,
        faults: 0,
        reclaimed: 0,
    });
    let _ = name;
    Ok(id)
}

/// Record that `pid` has the model at `base`. No pages are mapped yet: they
/// arrive when something touches them.
pub fn attach(id: u64, pid: usize, base: usize) -> Result<usize, &'static str> {
    let mut store = STORE.lock();
    let m = store.models.iter_mut().find(|m| m.id == id).ok_or("no such model")?;
    m.mappings.push(Mapping { pid, base });
    Ok(m.size)
}

/// Bring in the page of `id` that covers `offset`, and map it at `va` for `pid`.
///
/// Called from the page-fault path. Returns whether the fault was satisfied.
pub fn fault(id: u64, offset: usize, pid: usize, va: usize) -> bool {
    let index = offset / PAGE_SIZE;

    // Claim the page, wait for whoever already claimed it, or find it resident.
    loop {
        let state = {
            let mut store = STORE.lock();
            let Some(m) = store.models.iter_mut().find(|m| m.id == id) else {
                return false;
            };
            if index >= m.pages.len() {
                return false;
            }
            match m.pages[index] {
                PageState::Resident(f) => {
                    m.faults += 1;
                    PageState::Resident(f)
                }
                PageState::Loading => PageState::Loading,
                PageState::Absent => {
                    m.faults += 1;
                    m.pages[index] = PageState::Loading;
                    PageState::Absent
                }
            }
        };
        match state {
            // Already there: this fault costs a page-table entry and nothing
            // else, because another process paid to read it.
            PageState::Resident(frame) => {
                SHARED_HITS.fetch_add(1, Ordering::Relaxed);
                return map_into(pid, va, frame);
            }
            // Someone else is reading it. Wait rather than read it again.
            PageState::Loading => {
                crate::sched::yield_now();
                continue;
            }
            // Ours to read.
            PageState::Absent => break,
        }
    }

    // Read it from flash, outside the store lock: a disk read goes out to a
    // driver process and back, and holding a lock across that would serialise
    // every other model operation behind it.
    let (start_sector, size) = {
        let store = STORE.lock();
        let Some(m) = store.models.iter().find(|m| m.id == id) else {
            return false;
        };
        (m.start_sector, m.size)
    };

    let Some(frame) = frames::alloc() else {
        let mut store = STORE.lock();
        if let Some(m) = store.models.iter_mut().find(|m| m.id == id) {
            m.pages[index] = PageState::Absent;
        }
        return false;
    };
    let dst = phys_to_virt(frame) as *mut u8;
    unsafe { core::ptr::write_bytes(dst, 0, PAGE_SIZE) };

    let base_sector = start_sector + index as u64 * SECTORS_PER_PAGE;
    let mut sector = [0u8; blk::SECTOR_SIZE];
    for s in 0..SECTORS_PER_PAGE {
        let byte_off = index * PAGE_SIZE + s as usize * blk::SECTOR_SIZE;
        if byte_off >= size {
            break;
        }
        if blk::read_sector(base_sector + s, &mut sector).is_err() {
            frames::free(frame);
            let mut store = STORE.lock();
            if let Some(m) = store.models.iter_mut().find(|m| m.id == id) {
                m.pages[index] = PageState::Absent;
            }
            return false;
        }
        let n = (size - byte_off).min(blk::SECTOR_SIZE);
        unsafe {
            core::ptr::copy_nonoverlapping(
                sector.as_ptr(),
                dst.add(s as usize * blk::SECTOR_SIZE),
                n,
            )
        };
    }

    {
        let mut store = STORE.lock();
        let Some(m) = store.models.iter_mut().find(|m| m.id == id) else {
            frames::free(frame);
            return false;
        };
        m.pages[index] = PageState::Resident(frame);
        RESIDENT.fetch_add(1, Ordering::Relaxed);
    }

    map_into(pid, va, frame)
}

fn map_into(pid: usize, va: usize, frame: usize) -> bool {
    let Some(p) = crate::proc::get(pid) else { return false };
    unsafe { (*p).space.map(va, frame, 1, Perm::UserReadOnly).is_ok() }
}

/// Drop up to `want` resident pages, newest first, and return how many went.
///
/// Every page here is clean by construction, so reclaim is an unmap and a free
/// — no writeback, no swap file, no decision about what is dirty. That is the
/// property that makes weight pages the right thing to evict first.
pub fn reclaim(want: usize) -> usize {
    let mut freed = 0;
    let mut store = STORE.lock();

    for m in store.models.iter_mut() {
        for index in (0..m.pages.len()).rev() {
            if freed == want {
                break;
            }
            // A page being read right now is not a candidate: freeing it would
            // pull the frame out from under the thread filling it.
            let PageState::Resident(frame) = m.pages[index] else { continue };

            // Unmap from every process holding it first: a freed frame that is
            // still in someone's page table is the worst kind of bug.
            for map in m.mappings.iter() {
                if let Some(p) = crate::proc::get(map.pid) {
                    unsafe { (*p).space.unmap(map.base + index * PAGE_SIZE, 1) };
                }
            }
            frames::free(frame);
            m.pages[index] = PageState::Absent;
            m.reclaimed += 1;
            RESIDENT.fetch_sub(1, Ordering::Relaxed);
            freed += 1;
        }
    }
    freed
}

/// (id, hash, size, resident pages, total pages, faults, reclaimed, refs)
pub fn stat(id: u64) -> Option<(u64, u64, usize, usize, usize, u64, u64, usize)> {
    let store = STORE.lock();
    let m = store.models.iter().find(|m| m.id == id)?;
    Some((
        m.id,
        m.hash,
        m.size,
        m.resident_pages(),
        m.total_pages(),
        m.faults,
        m.reclaimed,
        m.refs,
    ))
}

/// Run `f` over every model, for reporting.
///
/// The closure runs with the store locked, so it must not call back into this
/// module — `stat` would take the same lock and wedge the core against itself.
/// Everything worth reporting is reachable from `&Model` without locking.
pub fn for_each(mut f: impl FnMut(&Model)) {
    let store = STORE.lock();
    for m in store.models.iter() {
        f(m);
    }
}

/// Total faults across every model, and how many were served from a page that
/// was already resident. Their difference is the number of pages actually read
/// from flash — the figure that says whether sharing happened.
pub fn fault_totals() -> (u64, u64) {
    let store = STORE.lock();
    let faults = store.models.iter().map(|m| m.faults).sum();
    (faults, SHARED_HITS.load(Ordering::Relaxed))
}

/// Pages of weights resident right now.
pub fn resident_pages() -> u64 {
    RESIDENT.load(Ordering::Relaxed)
}

/// Faults served from a frame that was already resident — pages that a
/// conventional OS would have read and stored a second time.
pub fn shared_hits() -> u64 {
    SHARED_HITS.load(Ordering::Relaxed)
}
