//! Block storage, as a client of a driver process.
//!
//! There is no driver here. `read_sector` packs a request, sends it on a
//! channel, and waits for a reply — the same thing any other process would do.
//! The kernel holds the channel capabilities because the filesystem happens to
//! live in the kernel today; when it moves out, this file goes with it and
//! nothing else changes.

use crate::{ipc, sched, time};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub const SECTOR_SIZE: usize = 512;

/// Wire protocol. Request: `[op][sector:8][data...]`. Reply: `[status][data...]`.
pub const OP_INFO: u8 = 0;
pub const OP_READ: u8 = 1;
pub const OP_WRITE: u8 = 2;
pub const OP_SHUTDOWN: u8 = 3;

const REPLY_TIMEOUT_TICKS: u64 = 300;

static REQ_CHANNEL: AtomicUsize = AtomicUsize::new(usize::MAX);
static REP_CHANNEL: AtomicUsize = AtomicUsize::new(usize::MAX);
static CAPACITY: AtomicU64 = AtomicU64::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);

/// Point the client at the channels a driver is listening on.
pub fn attach(request: usize, reply: usize) {
    REQ_CHANNEL.store(request, Ordering::Release);
    REP_CHANNEL.store(reply, Ordering::Release);
}

pub fn attached() -> bool {
    REQ_CHANNEL.load(Ordering::Acquire) != usize::MAX
}

fn round_trip(msg: Vec<u8>) -> Result<Vec<u8>, &'static str> {
    let req = REQ_CHANNEL.load(Ordering::Acquire);
    let rep = REP_CHANNEL.load(Ordering::Acquire);
    if req == usize::MAX {
        return Err("no block driver attached");
    }

    ipc::send(req, usize::MAX, msg).map_err(|_| "request channel is gone")?;
    REQUESTS.fetch_add(1, Ordering::Relaxed);

    // Yield rather than block: the driver is a process that has to be scheduled
    // to answer, and a driver that dies must cost an error rather than the
    // machine.
    let deadline = time::ticks() + REPLY_TIMEOUT_TICKS;
    loop {
        if let Some(m) = ipc::try_recv(rep) {
            if m.bytes.first() == Some(&0) {
                return Ok(m.bytes);
            }
            return Err("driver reported an error");
        }
        if time::ticks() > deadline {
            return Err("block driver did not answer");
        }
        sched::yield_now();
    }
}

/// Ask the driver what it attached to.
pub fn info() -> Result<u64, &'static str> {
    let reply = round_trip(vec![OP_INFO, 0, 0, 0, 0, 0, 0, 0, 0])?;
    if reply.len() < 9 {
        return Err("short info reply");
    }
    let mut c = [0u8; 8];
    c.copy_from_slice(&reply[1..9]);
    let capacity = u64::from_le_bytes(c);
    CAPACITY.store(capacity, Ordering::Relaxed);
    Ok(capacity)
}

pub fn capacity_sectors() -> u64 {
    CAPACITY.load(Ordering::Relaxed)
}

pub fn read_sector(sector: u64, out: &mut [u8]) -> Result<(), &'static str> {
    if out.len() != SECTOR_SIZE {
        return Err("reads are one sector");
    }
    let mut msg = vec![OP_READ];
    msg.extend_from_slice(&sector.to_le_bytes());
    let reply = round_trip(msg)?;
    if reply.len() < 1 + SECTOR_SIZE {
        return Err("short read reply");
    }
    out.copy_from_slice(&reply[1..1 + SECTOR_SIZE]);
    Ok(())
}

pub fn write_sector(sector: u64, data: &[u8]) -> Result<(), &'static str> {
    if data.len() != SECTOR_SIZE {
        return Err("writes are one sector");
    }
    let mut msg = vec![OP_WRITE];
    msg.extend_from_slice(&sector.to_le_bytes());
    msg.extend_from_slice(data);
    round_trip(msg)?;
    Ok(())
}

/// Ask the driver to exit, so the machine can shut down cleanly.
pub fn shutdown() {
    let mut msg = vec![OP_SHUTDOWN];
    msg.extend_from_slice(&0u64.to_le_bytes());
    let _ = round_trip(msg);
}

/// Requests sent since boot.
pub fn request_count() -> u64 {
    REQUESTS.load(Ordering::Relaxed)
}
