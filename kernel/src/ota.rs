//! A/B updates: two system slots, a boot counter, and a rollback rule.
//!
//! An update that can brick the device is not an update, it is a gamble. The
//! answer every phone converged on is the same one: keep two copies of the
//! system, install into the one you are not running, and boot the new one on
//! probation. If it never reports that it came up, go back.
//!
//! The whole mechanism is four rules, and the order of them is the mechanism:
//!
//! 1. An update is written to the *inactive* slot. Nothing that is running is
//!    ever overwritten, so an interrupted install costs a slot, not a device.
//! 2. Switching to it is a single sector write, so the switch either happened
//!    or did not. There is no state where half the pointer moved.
//! 3. **The try counter is decremented and written to disk before the attempt,
//!    not after.** This is the rule that makes the difference between a
//!    bootloop and a rollback: a system that decrements afterwards never
//!    decrements at all when the failure is a hang.
//! 4. Only the running system can mark itself good, and only after it has come
//!    up far enough to mean it. Until then the slot is on probation.
//!
//! **What is real here and what is not.** The state machine, the counter, the
//! durability ordering, the digest check before a slot is used, and the
//! rollback are real, and the exit test runs them across six actual power
//! cycles. What is *not* real is that the slots hold a kernel: there is no
//! bootloader here to hand control to a slot, so the "system image" is a
//! payload this kernel writes, verifies and checks the health of. Making the
//! slots hold the kernel itself means a bootloader that reads this control
//! block — U-Boot with a boot script, or a small first-stage of our own — and
//! that is a separate piece of work, not a bigger version of this one.

use crate::blk::{self, SECTOR_SIZE};
use crate::fs::{self, Fs, OTA_SLOT_SECTORS};
use crate::sha256;

const CB_MAGIC: u64 = 0x4A42_4F4F_5443_4230; // "JBOOTCB0"
const IMG_MAGIC: u64 = 0x4A53_5953_494D_4730; // "JSYSIMG0"

/// How many boots a new slot gets to prove itself.
///
/// Real systems use three to seven. Two is enough to show the rule and keeps
/// the exit test to six power cycles; nothing about the mechanism changes with
/// the number.
pub const MAX_TRIES: u8 = 2;

/// The synthetic system image: a header and a body of a pattern derived from
/// the version, so a slot holding the wrong version fails its digest check
/// rather than looking plausible.
const IMAGE_BYTES: usize = 4096;
const IMAGE_HEADER: usize = 32;

pub const FLAG_HEALTHY: u64 = 1;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SlotState {
    Empty,
    /// Written, switched to, and on probation.
    Unverified,
    /// Came up and said so.
    Successful,
    /// Ran out of tries.
    Failed,
}

impl SlotState {
    fn from(v: u8) -> SlotState {
        match v {
            1 => SlotState::Unverified,
            2 => SlotState::Successful,
            3 => SlotState::Failed,
            _ => SlotState::Empty,
        }
    }
    fn to(self) -> u8 {
        match self {
            SlotState::Empty => 0,
            SlotState::Unverified => 1,
            SlotState::Successful => 2,
            SlotState::Failed => 3,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            SlotState::Empty => "empty",
            SlotState::Unverified => "unverified",
            SlotState::Successful => "successful",
            SlotState::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Control {
    pub seq: u64,
    pub active: usize,
    pub tries: u8,
    pub rollbacks: u8,
    pub state: [SlotState; 2],
    pub version: [u64; 2],
    pub digest: [[u8; 32]; 2],
}

impl Control {
    pub fn blank() -> Control {
        Control {
            seq: 0,
            active: 0,
            tries: 0,
            rollbacks: 0,
            state: [SlotState::Empty; 2],
            version: [0; 2],
            digest: [[0; 32]; 2],
        }
    }

    pub fn other(&self) -> usize {
        1 - self.active
    }

    fn decode(raw: &[u8; SECTOR_SIZE]) -> Option<Control> {
        if u64::from_le_bytes(raw[0..8].try_into().ok()?) != CB_MAGIC || !fs::intact(raw) {
            return None;
        }
        let mut c = Control::blank();
        c.seq = u64::from_le_bytes(raw[8..16].try_into().ok()?);
        c.active = (raw[16] & 1) as usize;
        c.tries = raw[17];
        c.rollbacks = raw[18];
        c.state = [SlotState::from(raw[19]), SlotState::from(raw[20])];
        for i in 0..2 {
            c.version[i] = u64::from_le_bytes(raw[24 + i * 8..32 + i * 8].try_into().ok()?);
            c.digest[i].copy_from_slice(&raw[40 + i * 32..72 + i * 32]);
        }
        Some(c)
    }

    fn encode(&self) -> [u8; SECTOR_SIZE] {
        let mut raw = [0u8; SECTOR_SIZE];
        raw[0..8].copy_from_slice(&CB_MAGIC.to_le_bytes());
        raw[8..16].copy_from_slice(&self.seq.to_le_bytes());
        raw[16] = self.active as u8;
        raw[17] = self.tries;
        raw[18] = self.rollbacks;
        raw[19] = self.state[0].to();
        raw[20] = self.state[1].to();
        for i in 0..2 {
            raw[24 + i * 8..32 + i * 8].copy_from_slice(&self.version[i].to_le_bytes());
            raw[40 + i * 32..72 + i * 32].copy_from_slice(&self.digest[i]);
        }
        fs::seal(&mut raw);
        raw
    }
}

fn control_sector(f: &Fs) -> u64 {
    f.ota_start
}

fn slot_sector(f: &Fs, slot: usize) -> u64 {
    f.ota_start + 1 + slot as u64 * OTA_SLOT_SECTORS
}

pub fn read_control(f: &Fs) -> Option<Control> {
    let mut raw = [0u8; SECTOR_SIZE];
    blk::read_sector(control_sector(f), &mut raw).ok()?;
    Control::decode(&raw)
}

/// Persist the control block. One sector, sealed, so it either lands or does
/// not — the same assumption the filesystem's checkpoint makes, and the same
/// reason: a device guarantees a sector, not a range.
pub fn write_control(f: &Fs, c: &mut Control) -> Result<(), &'static str> {
    c.seq += 1;
    blk::write_sector(control_sector(f), &c.encode())
}

// ---- the synthetic system image -------------------------------------------

fn image_byte(version: u64, offset: usize) -> u8 {
    (offset.wrapping_mul(37) ^ (version as usize).wrapping_mul(1103)) as u8
}

/// Build a system image for `version`, healthy or not.
pub fn build_image(version: u64, healthy: bool, out: &mut [u8; IMAGE_BYTES]) {
    out.fill(0);
    out[0..8].copy_from_slice(&IMG_MAGIC.to_le_bytes());
    out[8..16].copy_from_slice(&version.to_le_bytes());
    out[16..24].copy_from_slice(&(if healthy { FLAG_HEALTHY } else { 0 }).to_le_bytes());
    out[24..32].copy_from_slice(&((IMAGE_BYTES - IMAGE_HEADER) as u64).to_le_bytes());
    for i in IMAGE_HEADER..IMAGE_BYTES {
        out[i] = image_byte(version, i);
    }
}

/// Write an image into a slot and record its digest. The slot written is
/// never the one running: that is the caller's job to arrange, and the only
/// reason an interrupted install is survivable.
pub fn stage(
    f: &Fs,
    c: &mut Control,
    slot: usize,
    version: u64,
    healthy: bool,
) -> Result<(), &'static str> {
    let mut img = [0u8; IMAGE_BYTES];
    build_image(version, healthy, &mut img);

    let base = slot_sector(f, slot);
    for s in 0..IMAGE_BYTES / SECTOR_SIZE {
        blk::write_sector(base + s as u64, &img[s * SECTOR_SIZE..(s + 1) * SECTOR_SIZE])?;
    }

    // Only once the bytes are down does the control block start pointing at
    // them. A crash before this leaves an unreferenced slot; a crash after it
    // leaves a slot that will be checked before it is trusted.
    c.version[slot] = version;
    c.digest[slot] = sha256::digest(&img);
    c.state[slot] = SlotState::Unverified;
    c.active = slot;
    c.tries = MAX_TRIES;
    write_control(f, c)
}

pub struct Image {
    pub version: u64,
    pub flags: u64,
    pub digest: [u8; 32],
}

/// Read a slot back and measure it. Nothing is trusted on the strength of the
/// control block saying it should be there.
pub fn load(f: &Fs, slot: usize) -> Result<Image, &'static str> {
    let mut img = [0u8; IMAGE_BYTES];
    let base = slot_sector(f, slot);
    for s in 0..IMAGE_BYTES / SECTOR_SIZE {
        blk::read_sector(base + s as u64, &mut img[s * SECTOR_SIZE..(s + 1) * SECTOR_SIZE])?;
    }
    if u64::from_le_bytes(img[0..8].try_into().unwrap()) != IMG_MAGIC {
        return Err("slot holds no system image");
    }
    Ok(Image {
        version: u64::from_le_bytes(img[8..16].try_into().unwrap()),
        flags: u64::from_le_bytes(img[16..24].try_into().unwrap()),
        digest: sha256::digest(&img),
    })
}

#[derive(PartialEq, Debug)]
pub enum Outcome {
    /// No control block: the device has never been updated.
    Fresh,
    /// The active slot is on probation and this boot is one of its tries.
    Trying { slot: usize, version: u64, tries_left: u8, healthy: bool },
    /// The slot ran out of tries and the other one was restored.
    RolledBack { from: usize, to: usize, version: u64 },
    /// Running a slot that has already proved itself.
    Running { slot: usize, version: u64 },
    /// The slot named by the control block does not hold what it should.
    Corrupt { slot: usize },
}

/// Decide what to boot, and record the decision before acting on it.
pub fn boot(f: &Fs) -> (Control, Outcome) {
    let Some(mut c) = read_control(f) else {
        return (Control::blank(), Outcome::Fresh);
    };

    // Out of tries: the slot had its chances. Go back to whichever slot last
    // said it came up, and mark this one so it is never chosen again without
    // being rewritten.
    if c.state[c.active] == SlotState::Unverified && c.tries == 0 {
        let from = c.active;
        let to = c.other();
        if c.state[to] == SlotState::Successful {
            c.state[from] = SlotState::Failed;
            c.active = to;
            c.rollbacks = c.rollbacks.saturating_add(1);
            let _ = write_control(f, &mut c);
            let version = c.version[to];
            return (c, Outcome::RolledBack { from, to, version });
        }
        // Nothing to fall back to. Keep trying rather than refuse to boot:
        // a device with one slot and a bad update is in trouble either way,
        // and a bootloop at least leaves a chance of being recovered.
        c.tries = 1;
        let _ = write_control(f, &mut c);
    }

    let slot = c.active;
    let img = match load(f, slot) {
        Ok(i) => i,
        Err(_) => return (c, Outcome::Corrupt { slot }),
    };
    if img.digest != c.digest[slot] || img.version != c.version[slot] {
        return (c, Outcome::Corrupt { slot });
    }

    if c.state[slot] == SlotState::Unverified {
        // Spend the try *now*. If this boot hangs or the power goes, the next
        // one must find one fewer try than this one did, and a counter written
        // after a successful boot is a counter that never moves when it
        // matters.
        c.tries = c.tries.saturating_sub(1);
        let _ = write_control(f, &mut c);
        let healthy = img.flags & FLAG_HEALTHY != 0;
        return (
            c,
            Outcome::Trying { slot, version: img.version, tries_left: c.tries, healthy },
        );
    }

    (c, Outcome::Running { slot, version: img.version })
}

/// The running system reporting that it came up. Only this turns probation
/// into a commitment.
pub fn mark_successful(f: &Fs, c: &mut Control) -> Result<(), &'static str> {
    c.state[c.active] = SlotState::Successful;
    c.tries = 0;
    // The slot we came from is no longer needed and is the next install's
    // target. Leaving it marked successful would make a rollback go somewhere
    // stale rather than nowhere.
    let other = c.other();
    if c.state[other] == SlotState::Successful {
        c.state[other] = SlotState::Empty;
    }
    write_control(f, c)
}

/// Set up a device that has never been updated: slot A holds version 1 and
/// has already proved itself, because it is what is running.
pub fn initialise(f: &Fs) -> Result<Control, &'static str> {
    let mut c = Control::blank();
    let mut img = [0u8; IMAGE_BYTES];
    build_image(1, true, &mut img);
    let base = slot_sector(f, 0);
    for s in 0..IMAGE_BYTES / SECTOR_SIZE {
        blk::write_sector(base + s as u64, &img[s * SECTOR_SIZE..(s + 1) * SECTOR_SIZE])?;
    }
    c.version[0] = 1;
    c.digest[0] = sha256::digest(&img);
    c.state[0] = SlotState::Successful;
    c.active = 0;
    c.tries = 0;
    write_control(f, &mut c)?;
    Ok(c)
}

pub fn slot_name(slot: usize) -> &'static str {
    if slot == 0 {
        "A"
    } else {
        "B"
    }
}
