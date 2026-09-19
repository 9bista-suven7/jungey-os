//! JLFS — a small log-structured filesystem.
//!
//! Log-structured for two reasons that both matter later. Flash cannot be
//! overwritten in place, so every real mobile filesystem is log-structured
//! somewhere; and an append-only log with an atomically switched root is the
//! cheapest honest way to get crash consistency, which stage 6's agent action
//! log needs to be worth anything. "What did the assistant do, and can I undo
//! it" is not answerable on a filesystem that can lose the middle of a write.
//!
//! On-disk shape:
//!
//! ```text
//!   sector 0      superblock          static after format
//!   sector 1      checkpoint A    \   alternately written; the newer valid
//!   sector 2      checkpoint B    /   one is the filesystem's root
//!   sector 4      test state          used by the crash test, not by the fs
//!   sector 8..    log                 append-only data
//! ```
//!
//! Committing is one sector write. Data is appended to the log first and the
//! checkpoint second, so a crash either loses the checkpoint — and with it every
//! trace of the unfinished write — or lands after it, with the data already
//! durable. There is no order in which a reader sees half of one.
//!
//! The disk is reached through `blk`, which is a client of a driver process.
//! Nothing in this file knows what kind of device is underneath, or that the
//! code driving it runs outside the kernel.
//!
//! **Assumption:** a single sector write is atomic — it lands whole or not at
//! all. That is the same assumption every journalling filesystem makes, and the
//! checkpoint CRC catches the case where the hardware breaks its promise.

use crate::blk::{self, SECTOR_SIZE};

const SB_SECTOR: u64 = 0;

/// Sectors reserved at the top of the disk for the KV cache to spill into.
///
/// Carved out explicitly and recorded in the superblock, so the log *refuses*
/// to grow into it. The alternative — assuming the log will never get that far
/// — is exactly the assumption that had an earlier version of this project
/// quietly overwriting its own model file.
const SPILL_SECTORS: u64 = 16 * 1024; // 8 MiB

/// Sectors reserved for the agent's action log, below the spill region.
///
/// Its own region for the same reason the spill area has one: a log that
/// records what an agent did is worthless if the thing it is auditing can grow
/// over it.
const AUDIT_SECTORS: u64 = 2048; // 1 MiB
const CP_SECTORS: [u64; 2] = [1, 2];
pub const TEST_STATE_SECTOR: u64 = 4;
const LOG_START: u64 = 8;

const SB_MAGIC: u64 = 0x4A4C_4653_5F53_4230; // "JLFS_SB0"
const CP_MAGIC: u64 = 0x4A4C_4653_5F43_5030; // "JLFS_CP0"

/// Files per checkpoint. A flat directory that fits in one sector: the log and
/// the atomic root swap are what this stage is about, not a B-tree.
pub const MAX_FILES: usize = 8;
pub const MAX_NAME: usize = 28;

const ENTRY_SIZE: usize = 48;
const ENTRIES_OFF: usize = 32;
const CRC_OFF: usize = SECTOR_SIZE - 4;

// ---- little-endian field helpers ------------------------------------------
// The on-disk format is written out by hand rather than by casting a struct:
// layout is part of the format, and a silent change to a Rust struct must not
// be able to change what is on someone's disk.

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
fn get_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn get_u64(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

/// CRC-32 (IEEE), computed bitwise. No table: 512 bytes a few times per commit
/// is not worth a kilobyte of kernel rodata.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Stamp a sector's trailing CRC over everything before it.
fn seal(sector: &mut [u8; SECTOR_SIZE]) {
    let c = crc32(&sector[..CRC_OFF]);
    put_u32(sector, CRC_OFF, c);
}

/// Does a sector's CRC match its contents?
fn intact(sector: &[u8; SECTOR_SIZE]) -> bool {
    crc32(&sector[..CRC_OFF]) == get_u32(sector, CRC_OFF)
}

#[derive(Clone, Copy)]
pub struct FileEntry {
    pub name: [u8; MAX_NAME],
    pub size: u32,
    pub start: u64,
    pub sectors: u32,
}

impl FileEntry {
    const fn empty() -> Self {
        FileEntry { name: [0; MAX_NAME], size: 0, start: 0, sectors: 0 }
    }

    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(MAX_NAME);
        core::str::from_utf8(&self.name[..end]).unwrap_or("<invalid>")
    }
}

pub struct Checkpoint {
    pub seq: u64,
    pub log_head: u64,
    pub files: [FileEntry; MAX_FILES],
    pub count: usize,
}

impl Checkpoint {
    fn new() -> Self {
        Checkpoint {
            seq: 1,
            log_head: LOG_START,
            files: [FileEntry::empty(); MAX_FILES],
            count: 0,
        }
    }

    fn decode(raw: &[u8; SECTOR_SIZE]) -> Option<Checkpoint> {
        if get_u64(raw, 0) != CP_MAGIC || !intact(raw) {
            return None;
        }
        let count = get_u32(raw, 24) as usize;
        if count > MAX_FILES {
            return None;
        }
        let mut cp = Checkpoint {
            seq: get_u64(raw, 8),
            log_head: get_u64(raw, 16),
            files: [FileEntry::empty(); MAX_FILES],
            count,
        };
        for i in 0..count {
            let at = ENTRIES_OFF + i * ENTRY_SIZE;
            cp.files[i].name.copy_from_slice(&raw[at..at + MAX_NAME]);
            cp.files[i].size = get_u32(raw, at + 28);
            cp.files[i].start = get_u64(raw, at + 32);
            cp.files[i].sectors = get_u32(raw, at + 40);
        }
        Some(cp)
    }

    fn encode(&self) -> [u8; SECTOR_SIZE] {
        let mut raw = [0u8; SECTOR_SIZE];
        put_u64(&mut raw, 0, CP_MAGIC);
        put_u64(&mut raw, 8, self.seq);
        put_u64(&mut raw, 16, self.log_head);
        put_u32(&mut raw, 24, self.count as u32);
        for i in 0..self.count {
            let at = ENTRIES_OFF + i * ENTRY_SIZE;
            raw[at..at + MAX_NAME].copy_from_slice(&self.files[i].name);
            put_u32(&mut raw, at + 28, self.files[i].size);
            put_u64(&mut raw, at + 32, self.files[i].start);
            put_u32(&mut raw, at + 40, self.files[i].sectors);
        }
        seal(&mut raw);
        raw
    }
}

pub struct Fs {
    pub cp: Checkpoint,
    /// First sector of the reserved spill region: the log stops here.
    pub spill_start: u64,
    pub spill_sectors: u64,
    pub audit_start: u64,
    pub audit_sectors: u64,
    /// Which checkpoint slot the live root is in. The next commit writes the
    /// other one, so a torn write can never damage the root we booted from.
    pub slot: usize,
    pub total_sectors: u64,
}

/// Lay down a fresh filesystem: superblock, then one valid checkpoint.
pub fn format(total_sectors: u64) -> Result<(), &'static str> {
    let mut sb = [0u8; SECTOR_SIZE];
    put_u64(&mut sb, 0, SB_MAGIC);
    put_u32(&mut sb, 8, 1); // version
    put_u64(&mut sb, 12, total_sectors);
    let spill_start = total_sectors - SPILL_SECTORS;
    let audit_start = spill_start - AUDIT_SECTORS;
    put_u64(&mut sb, 20, LOG_START);
    put_u64(&mut sb, 28, audit_start - LOG_START);
    put_u64(&mut sb, 36, spill_start);
    put_u64(&mut sb, 44, SPILL_SECTORS);
    put_u64(&mut sb, 52, audit_start);
    put_u64(&mut sb, 60, AUDIT_SECTORS);
    seal(&mut sb);
    blk::write_sector(SB_SECTOR, &sb)?;

    // Slot B is left invalid on purpose: mount must cope with one good
    // checkpoint and one that has never been written.
    let blank = [0u8; SECTOR_SIZE];
    blk::write_sector(CP_SECTORS[1], &blank)?;
    blk::write_sector(CP_SECTORS[0], &Checkpoint::new().encode())?;

    // Start the action log empty. Its scan stops at the first unwritten slot,
    // so clearing the first sector is enough — and leaving stale records from a
    // previous filesystem would have the new one's log start mid-chain.
    blk::write_sector(audit_start, &blank)?;
    Ok(())
}

/// Read the superblock and adopt the newer of the two valid checkpoints.
pub fn mount() -> Result<Fs, &'static str> {
    let mut sb = [0u8; SECTOR_SIZE];
    blk::read_sector(SB_SECTOR, &mut sb)?;
    if get_u64(&sb, 0) != SB_MAGIC {
        return Err("not a JLFS filesystem");
    }
    if !intact(&sb) {
        return Err("superblock checksum mismatch");
    }
    let total_sectors = get_u64(&sb, 12);
    let spill_start = get_u64(&sb, 36);
    let spill_sectors = get_u64(&sb, 44);
    let audit_start = get_u64(&sb, 52);
    let audit_sectors = get_u64(&sb, 60);

    let mut best: Option<(usize, Checkpoint)> = None;
    for (i, &sector) in CP_SECTORS.iter().enumerate() {
        let mut raw = [0u8; SECTOR_SIZE];
        blk::read_sector(sector, &mut raw)?;
        // A slot that fails to decode is not an error: it is either never
        // written, or the half-written casualty of a crash. That is exactly the
        // case the two-slot scheme exists to survive.
        if let Some(cp) = Checkpoint::decode(&raw) {
            if best.as_ref().map_or(true, |(_, b)| cp.seq > b.seq) {
                best = Some((i, cp));
            }
        }
    }

    let (slot, cp) = best.ok_or("no valid checkpoint: filesystem is unrecoverable")?;
    Ok(Fs { cp, slot, total_sectors, spill_start, spill_sectors, audit_start, audit_sectors })
}

impl Fs {
    pub fn files(&self) -> &[FileEntry] {
        &self.cp.files[..self.cp.count]
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.files().iter().position(|f| f.name_str() == name)
    }

    pub fn read(&self, name: &str, out: &mut [u8]) -> Result<usize, &'static str> {
        let i = self.find(name).ok_or("no such file")?;
        let entry = self.cp.files[i];
        let size = entry.size as usize;
        if out.len() < size {
            return Err("buffer too small");
        }
        let mut sector = [0u8; SECTOR_SIZE];
        let mut done = 0;
        for s in 0..entry.sectors as u64 {
            blk::read_sector(entry.start + s, &mut sector)?;
            let n = (size - done).min(SECTOR_SIZE);
            out[done..done + n].copy_from_slice(&sector[..n]);
            done += n;
            if done == size {
                break;
            }
        }
        Ok(size)
    }

    /// Append `data` to the log and make it visible as `name`.
    ///
    /// `crash_after` powers the machine off once that many data sectors have
    /// landed, which is the whole crash test. `Some(0)` through `Some(n-1)` cut
    /// power part way through the data; `Some(n)` cuts it with every byte on
    /// the disk and only the checkpoint missing — the case that distinguishes a
    /// filesystem that is crash-consistent from one that merely looks tidy.
    pub fn write(
        &mut self,
        name: &str,
        data: &[u8],
        crash_after: Option<u32>,
    ) -> Result<(), &'static str> {
        if name.len() > MAX_NAME {
            return Err("name too long");
        }
        let sectors = data.len().div_ceil(SECTOR_SIZE) as u32;
        let start = self.cp.log_head;
        // The log stops at the reserved regions rather than at the end of the
        // disk: other subsystems own those sectors.
        if start + sectors as u64 > self.audit_start {
            return Err("log is full");
        }

        // Step one: the data. Nothing points at it yet, so a crash here costs
        // only the space.
        let mut sector = [0u8; SECTOR_SIZE];
        for s in 0..sectors as usize {
            if crash_after == Some(s as u32) {
                crate::println!(
                    "  crash      : power cut after {} of {} data sectors",
                    s, sectors
                );
                crate::smp::system_off();
            }
            let from = s * SECTOR_SIZE;
            let n = (data.len() - from).min(SECTOR_SIZE);
            sector[..n].copy_from_slice(&data[from..from + n]);
            sector[n..].fill(0);
            blk::write_sector(start + s as u64, &sector)?;
        }

        if crash_after == Some(sectors) {
            crate::println!(
                "  crash      : power cut with all {} data sectors written, checkpoint skipped",
                sectors
            );
            crate::smp::system_off();
        }

        // Step two: the root. One sector, so it happens or it does not.
        let mut next = Checkpoint {
            seq: self.cp.seq + 1,
            log_head: start + sectors as u64,
            files: self.cp.files,
            count: self.cp.count,
        };
        let idx = match self.find(name) {
            Some(i) => i,
            None => {
                if next.count == MAX_FILES {
                    return Err("directory is full");
                }
                next.count += 1;
                next.count - 1
            }
        };
        next.files[idx] = FileEntry::empty();
        next.files[idx].name[..name.len()].copy_from_slice(name.as_bytes());
        next.files[idx].size = data.len() as u32;
        next.files[idx].start = start;
        next.files[idx].sectors = sectors;

        let target = 1 - self.slot;
        blk::write_sector(CP_SECTORS[target], &next.encode())?;
        self.cp = next;
        self.slot = target;
        Ok(())
    }

    /// Replace or remove a directory entry, committing a new checkpoint.
    ///
    /// This is what makes undo cheap. A log-structured filesystem never
    /// overwrites, so the bytes a file used to contain are still on the disk
    /// after it is rewritten — putting the old entry back is a metadata
    /// operation, not a copy. The absence of a cleaner, which is a shortcoming
    /// everywhere else, is what buys it here.
    pub fn set_entry(&mut self, name: &str, entry: Option<FileEntry>) -> Result<(), &'static str> {
        let mut next = Checkpoint {
            seq: self.cp.seq + 1,
            log_head: self.cp.log_head,
            files: self.cp.files,
            count: self.cp.count,
        };
        let at = self.find(name);
        match (at, entry) {
            (Some(i), Some(e)) => next.files[i] = e,
            (Some(i), None) => {
                // Remove by shifting the tail down; order is not meaningful.
                for j in i..next.count - 1 {
                    next.files[j] = next.files[j + 1];
                }
                next.count -= 1;
                next.files[next.count] = FileEntry::empty();
            }
            (None, Some(e)) => {
                if next.count == MAX_FILES {
                    return Err("directory is full");
                }
                next.files[next.count] = e;
                next.count += 1;
            }
            (None, None) => return Ok(()),
        }

        let target = 1 - self.slot;
        blk::write_sector(CP_SECTORS[target], &next.encode())?;
        self.cp = next;
        self.slot = target;
        Ok(())
    }

    /// The directory entry for `name`, if it exists.
    pub fn entry(&self, name: &str) -> Option<FileEntry> {
        self.find(name).map(|i| self.cp.files[i])
    }

    /// Sectors of log in use, and how much of that is garbage left by rewrites.
    ///
    /// Nothing reclaims it yet: a cleaner is the other half of a log-structured
    /// filesystem, and it wants a real workload to be tuned against.
    pub fn usage(&self) -> (u64, u64) {
        let used = self.cp.log_head - LOG_START;
        let live: u64 = self.files().iter().map(|f| f.sectors as u64).sum();
        (used, used - live)
    }
}

// ---- test-state sector ----------------------------------------------------
// Outside the filesystem on purpose: the crash test has to survive the
// filesystem being inconsistent, so it cannot live inside it.

const TEST_MAGIC: u64 = 0x4A4C_4653_5445_5354; // "JLFSTEST"

pub fn read_phase() -> u64 {
    let mut raw = [0u8; SECTOR_SIZE];
    if blk::read_sector(TEST_STATE_SECTOR, &mut raw).is_err() {
        return 0;
    }
    if get_u64(&raw, 0) != TEST_MAGIC || !intact(&raw) {
        return 0;
    }
    get_u64(&raw, 8)
}

pub fn write_phase(phase: u64) -> Result<(), &'static str> {
    let mut raw = [0u8; SECTOR_SIZE];
    put_u64(&mut raw, 0, TEST_MAGIC);
    put_u64(&mut raw, 8, phase);
    seal(&mut raw);
    blk::write_sector(TEST_STATE_SECTOR, &raw)
}
