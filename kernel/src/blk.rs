//! Block storage, as a client of a driver process.
//!
//! There is no driver here. `read_sector` packs a request, sends it on a
//! channel, and waits for a reply — the same thing any other process would do.
//! The kernel holds the channel capabilities because the filesystem happens to
//! live in the kernel today; when it moves out, this file goes with it and
//! nothing else changes.

use crate::{ipc, sched, time};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub const SECTOR_SIZE: usize = 512;

/// Wire protocol.
///
///   request  `[op:1][tag:4][sector:8][data...]`
///   reply    `[status:1][tag:4][data...]`
///
/// The tag is not decoration. Without it, a request that times out leaves its
/// reply in the channel, the next request picks that up as its own, and every
/// answer from then on is one behind — which does not look like a protocol bug.
/// It looks like the disk returning the wrong sector, intermittently, for the
/// rest of the boot.
pub const OP_INFO: u8 = 0;
pub const OP_READ: u8 = 1;
pub const OP_WRITE: u8 = 2;
pub const OP_SHUTDOWN: u8 = 3;

/// Longer than the driver's own worst case for one request, so a slow device
/// does not cause a timeout the driver never learns about.
const REPLY_TIMEOUT_TICKS: u64 = 800;

const HEADER: usize = 13; // op, tag, sector
const REPLY_HEADER: usize = 5; // status, tag

static REQ_CHANNEL: AtomicUsize = AtomicUsize::new(usize::MAX);
static REP_CHANNEL: AtomicUsize = AtomicUsize::new(usize::MAX);
static CAPACITY: AtomicU64 = AtomicU64::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static NEXT_TAG: AtomicU32 = AtomicU32::new(1);
static STALE: AtomicU64 = AtomicU64::new(0);

/// Point the client at the channels a driver is listening on.
pub fn attach(request: usize, reply: usize) {
    REQ_CHANNEL.store(request, Ordering::Release);
    REP_CHANNEL.store(reply, Ordering::Release);
}

pub fn attached() -> bool {
    REQ_CHANNEL.load(Ordering::Acquire) != usize::MAX
}

/// One request may be outstanding at a time.
///
/// Not a `SpinLock`: the wait for a reply yields, and a lock that masks
/// interrupts cannot be held across a yield. This is a sleeping mutex built the
/// only way one can be built here — spin on the flag, but give up the CPU
/// between attempts.
///
/// Without it two threads interleave on the same reply channel and take each
/// other's answers, which does not look like a lock bug. It looks like the disk
/// returning wrong data.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

struct Outstanding;

impl Outstanding {
    fn acquire() -> Outstanding {
        while IN_FLIGHT
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            sched::yield_now();
        }
        Outstanding
    }
}

impl Drop for Outstanding {
    fn drop(&mut self) {
        IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Build `[op][tag][sector]`, leaving the caller to append data.
fn header(op: u8, tag: u32, sector: u64) -> Vec<u8> {
    let mut m = Vec::with_capacity(HEADER);
    m.push(op);
    m.extend_from_slice(&tag.to_le_bytes());
    m.extend_from_slice(&sector.to_le_bytes());
    m
}

fn reply_tag(bytes: &[u8]) -> u32 {
    if bytes.len() < REPLY_HEADER {
        return 0;
    }
    u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]])
}

fn round_trip(msg: Vec<u8>, tag: u32) -> Result<Vec<u8>, &'static str> {
    let msg_op = msg[0];
    let msg_sector = u64::from_le_bytes([
        msg[5], msg[6], msg[7], msg[8], msg[9], msg[10], msg[11], msg[12],
    ]);
    let _guard = Outstanding::acquire();
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
            // A reply to a request we already gave up on. Drop it, or it
            // becomes the answer to this one.
            if reply_tag(&m.bytes) != tag {
                STALE.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if m.bytes.first() == Some(&0) {
                return Ok(m.bytes);
            }
            return Err("driver reported an error");
        }
        if time::ticks() > deadline {
            let (rs, rr, rq) = ipc::stats(req);
            let (ps, pr, pq) = ipc::stats(rep);
            crate::println!(
                "  blk        : timeout op {} sector {} tag {} tick {} | req sent {} recv {} queued {} | rep sent {} recv {} queued {}",
                msg_op, msg_sector, tag, time::ticks(), rs, rr, rq, ps, pr, pq
            );
            sched::dump("block driver did not answer");
            return Err("block driver did not answer");
        }
        sched::yield_now();
    }
}

fn next_tag() -> u32 {
    NEXT_TAG.fetch_add(1, Ordering::Relaxed)
}

/// Ask the driver what it attached to.
pub fn info() -> Result<u64, &'static str> {
    let tag = next_tag();
    let reply = round_trip(header(OP_INFO, tag, 0), tag)?;
    if reply.len() < REPLY_HEADER + 8 {
        return Err("short info reply");
    }
    let mut c = [0u8; 8];
    c.copy_from_slice(&reply[REPLY_HEADER..REPLY_HEADER + 8]);
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
    let tag = next_tag();
    let reply = round_trip(header(OP_READ, tag, sector), tag)?;
    if reply.len() < REPLY_HEADER + SECTOR_SIZE {
        return Err("short read reply");
    }
    out.copy_from_slice(&reply[REPLY_HEADER..REPLY_HEADER + SECTOR_SIZE]);
    Ok(())
}

pub fn write_sector(sector: u64, data: &[u8]) -> Result<(), &'static str> {
    if data.len() != SECTOR_SIZE {
        return Err("writes are one sector");
    }
    let tag = next_tag();
    let mut msg = header(OP_WRITE, tag, sector);
    msg.extend_from_slice(data);
    round_trip(msg, tag)?;
    Ok(())
}

/// Ask the driver to exit, so the machine can shut down cleanly.
pub fn shutdown() {
    let tag = next_tag();
    let _ = round_trip(header(OP_SHUTDOWN, tag, 0), tag);
}

/// Replies discarded because they answered a request that had already timed
/// out. Should be zero; anything else means the driver is running late.
pub fn stale_replies() -> u64 {
    STALE.load(Ordering::Relaxed)
}

/// Requests sent since boot.
pub fn request_count() -> u64 {
    REQUESTS.load(Ordering::Relaxed)
}
