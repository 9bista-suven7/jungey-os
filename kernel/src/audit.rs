//! The action log: an append-only, hash-chained record of what was done and on
//! whose authority.
//!
//! Stage 6's claim is that an agent can act for you without acting *as* you.
//! Capabilities are half of that — they bound what it can reach. This is the
//! other half: afterwards, you can find out what it actually did. "What did the
//! assistant do last Tuesday, and under what authority" is not answerable by a
//! permission model alone, because a permission model records the shape of
//! authority and not its exercise.
//!
//! Each record carries the hash of the one before it, so a record cannot be
//! altered or removed without breaking every hash after it. Verification walks
//! the chain and reports the first break, which is the seq number of the
//! earliest record that is no longer trustworthy.
//!
//! **This is tamper-evident, not tamper-proof, and the difference matters.**
//! The hash is FNV-1a — fast, unkeyed, and reversible by anyone who can write
//! to the disk, because they can simply recompute the rest of the chain. It
//! catches corruption and casual editing. Catching a determined attacker needs
//! a keyed MAC with the key in secure storage, or an append-only device, and
//! neither exists yet on this machine. Saying so here is better than implying a
//! guarantee the code does not provide.

use crate::blk;
use crate::sync::SpinLock;
use crate::time;
use core::sync::atomic::{AtomicU64, Ordering};

/// Four records to a sector, so appending is one read-modify-write.
pub const RECORD_BYTES: usize = 128;
const RECORDS_PER_SECTOR: u64 = (blk::SECTOR_SIZE / RECORD_BYTES) as u64;
const NAME_BYTES: usize = 32;
const DETAIL_BYTES: usize = 40;

const MAGIC: u64 = 0x4A5F_4155_4449_5430; // "J_AUDIT0"

/// Where a field lives inside a record.
const OFF_MAGIC: usize = 0;
const OFF_SEQ: usize = 8;
const OFF_PREV: usize = 16;
const OFF_TICK: usize = 24;
const OFF_ACTOR: usize = 32;
const OFF_CAP: usize = 40;
const OFF_NAME: usize = 48;
const OFF_DETAIL: usize = 48 + NAME_BYTES;
const OFF_HASH: usize = RECORD_BYTES - 8;

struct Log {
    start: u64,
    sectors: u64,
    next_seq: u64,
    last_hash: u64,
    ready: bool,
}

static LOG: SpinLock<Log> =
    SpinLock::new(Log { start: 0, sectors: 0, next_seq: 0, last_hash: 0, ready: false });

static APPENDED: AtomicU64 = AtomicU64::new(0);

fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn get_u64(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

/// FNV-1a over a record's bytes, seeded with the previous record's hash. Not
/// cryptographic; see the module comment.
fn hash_record(rec: &[u8], prev: u64) -> u64 {
    let mut h = prev ^ 0xcbf2_9ce4_8422_2325;
    for &b in &rec[..OFF_HASH] {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// Point the log at the region the filesystem reserved for it, and find the end
/// of whatever is already there.
pub fn init(start: u64, sectors: u64) {
    let mut seq = 0;
    let mut last_hash = 0;
    let mut sector_buf = [0u8; blk::SECTOR_SIZE];
    let capacity = sectors * RECORDS_PER_SECTOR;

    'scan: for s in 0..sectors {
        if blk::read_sector(start + s, &mut sector_buf).is_err() {
            break;
        }
        for r in 0..RECORDS_PER_SECTOR as usize {
            let rec = &sector_buf[r * RECORD_BYTES..(r + 1) * RECORD_BYTES];
            if get_u64(rec, OFF_MAGIC) != MAGIC {
                break 'scan; // first unwritten slot: the end of the log
            }
            seq = get_u64(rec, OFF_SEQ) + 1;
            last_hash = get_u64(rec, OFF_HASH);
        }
    }

    let mut log = LOG.lock();
    log.start = start;
    log.sectors = sectors;
    log.next_seq = seq.min(capacity);
    log.last_hash = last_hash;
    log.ready = true;
}

/// Record something that happened.
///
/// `actor` is who did it, `cap` the capability it was done under, `name` what
/// was done and `detail` to what. The capability id is what ties a record back
/// to a delegation chain.
pub fn append(actor: &str, cap: u64, name: &str, detail: &str) -> Result<u64, &'static str> {
    let (start, seq, prev, sectors) = {
        let log = LOG.lock();
        if !log.ready {
            return Err("no audit log");
        }
        (log.start, log.next_seq, log.last_hash, log.sectors)
    };
    if seq >= sectors * RECORDS_PER_SECTOR {
        return Err("audit log is full");
    }

    let mut rec = [0u8; RECORD_BYTES];
    put_u64(&mut rec, OFF_MAGIC, MAGIC);
    put_u64(&mut rec, OFF_SEQ, seq);
    put_u64(&mut rec, OFF_PREV, prev);
    put_u64(&mut rec, OFF_TICK, time::ticks());
    put_u64(&mut rec, OFF_ACTOR, actor.len() as u64);
    put_u64(&mut rec, OFF_CAP, cap);
    let n = name.as_bytes().len().min(NAME_BYTES);
    rec[OFF_NAME..OFF_NAME + n].copy_from_slice(&name.as_bytes()[..n]);
    let d = detail.as_bytes().len().min(DETAIL_BYTES);
    rec[OFF_DETAIL..OFF_DETAIL + d].copy_from_slice(&detail.as_bytes()[..d]);
    let hash = hash_record(&rec, prev);
    put_u64(&mut rec, OFF_HASH, hash);

    // Read-modify-write the sector this record shares with up to three others.
    let sector = start + seq / RECORDS_PER_SECTOR;
    let slot = (seq % RECORDS_PER_SECTOR) as usize;
    let mut buf = [0u8; blk::SECTOR_SIZE];
    blk::read_sector(sector, &mut buf)?;
    buf[slot * RECORD_BYTES..(slot + 1) * RECORD_BYTES].copy_from_slice(&rec);
    blk::write_sector(sector, &buf)?;

    let mut log = LOG.lock();
    log.next_seq = seq + 1;
    log.last_hash = hash;
    APPENDED.fetch_add(1, Ordering::Relaxed);
    Ok(seq)
}

pub struct Verified {
    pub records: u64,
    /// The sequence number of the earliest record that no longer verifies.
    pub first_bad: Option<u64>,
}

/// Walk the chain from the beginning and check every link.
pub fn verify() -> Verified {
    let (start, count) = {
        let log = LOG.lock();
        (log.start, log.next_seq)
    };
    let mut prev = 0u64;
    let mut buf = [0u8; blk::SECTOR_SIZE];
    let mut checked = 0;

    for seq in 0..count {
        let sector = start + seq / RECORDS_PER_SECTOR;
        let slot = (seq % RECORDS_PER_SECTOR) as usize;
        if seq % RECORDS_PER_SECTOR == 0 && blk::read_sector(sector, &mut buf).is_err() {
            return Verified { records: checked, first_bad: Some(seq) };
        }
        let rec = &buf[slot * RECORD_BYTES..(slot + 1) * RECORD_BYTES];

        // Three ways a record can be wrong: it is not a record, it does not
        // follow the one before it, or its own contents do not match its hash.
        let stored = get_u64(rec, OFF_HASH);
        if get_u64(rec, OFF_MAGIC) != MAGIC
            || get_u64(rec, OFF_PREV) != prev
            || hash_record(rec, prev) != stored
        {
            return Verified { records: checked, first_bad: Some(seq) };
        }
        prev = stored;
        checked += 1;
    }
    Verified { records: checked, first_bad: None }
}

/// Read one record back: (seq, tick, cap, name, detail).
pub fn read(seq: u64, name_out: &mut [u8; NAME_BYTES], detail_out: &mut [u8; DETAIL_BYTES])
    -> Option<(u64, u64, u64)>
{
    let start = LOG.lock().start;
    let sector = start + seq / RECORDS_PER_SECTOR;
    let slot = (seq % RECORDS_PER_SECTOR) as usize;
    let mut buf = [0u8; blk::SECTOR_SIZE];
    blk::read_sector(sector, &mut buf).ok()?;
    let rec = &buf[slot * RECORD_BYTES..(slot + 1) * RECORD_BYTES];
    if get_u64(rec, OFF_MAGIC) != MAGIC {
        return None;
    }
    name_out.copy_from_slice(&rec[OFF_NAME..OFF_NAME + NAME_BYTES]);
    detail_out.copy_from_slice(&rec[OFF_DETAIL..OFF_DETAIL + DETAIL_BYTES]);
    Some((get_u64(rec, OFF_SEQ), get_u64(rec, OFF_TICK), get_u64(rec, OFF_CAP)))
}

pub fn as_str(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).unwrap_or("<invalid>")
}

pub fn len() -> u64 {
    LOG.lock().next_seq
}

/// Corrupt one record on disk, to prove the chain notices.
///
/// Only the test calls this. It exists because a tamper-evident log that has
/// never been shown to detect tampering is a claim, not a property.
pub fn corrupt_for_test(seq: u64, byte: usize) -> Result<(), &'static str> {
    let start = LOG.lock().start;
    let sector = start + seq / RECORDS_PER_SECTOR;
    let slot = (seq % RECORDS_PER_SECTOR) as usize;
    let mut buf = [0u8; blk::SECTOR_SIZE];
    blk::read_sector(sector, &mut buf)?;
    let at = slot * RECORD_BYTES + byte.min(OFF_HASH - 1);
    buf[at] ^= 0xff;
    blk::write_sector(sector, &buf)
}

pub fn appended() -> u64 {
    APPENDED.load(Ordering::Relaxed)
}
