//! virtio-blk driver — in userspace.
//!
//! An ordinary process. It has no privilege, no kernel mapping and no way to
//! reach a device it was not handed: its authority is four capabilities — the
//! device's registers, the device's interrupt, a DMA region the device can
//! reach, and the two channels it answers on. Revoking them stops it as surely
//! as killing it, and a bug in it faults one process.
//!
//! What it does *not* get is the ability to find another device. There is no
//! argument to `map_device` naming a physical address; the address comes from
//! the capability.
//!
//! Spec: virtio 1.2, sections 4.2 (MMIO) and 5.2 (block device).

use crate::say;
use crate::sys::*;

// Capability slots the kernel fills before starting us.
const CAP_REQ: usize = 0; // channel, recv: requests arrive here
const CAP_REP: usize = 1; // channel, send: replies go back
const CAP_MMIO: usize = 2; // the device's registers
const CAP_IRQ: usize = 3; // the device's interrupt
const CAP_DMA: usize = 4; // memory the device can reach

/// Where we choose to put the things we were given. Our address space, our
/// choice — the capability decides *what*, we decide *where*.
///
/// `map_device` returns the registers' offset within the page it mapped,
/// because virtio-mmio spaces its transports 0x200 apart and an MMU cannot
/// isolate a sub-page window. We are handed the whole page and told where in it
/// to look.
const MMIO_PAGE: usize = 0x1000_0000;
const DMA_VA: usize = 0x1100_0000;

/// Set once by `attach`, from the offset `map_device` returns.
static mut MMIO_VA: usize = 0;

const PAGE: usize = 4096;
pub const SECTOR: usize = 512;

// ---- virtio-mmio registers ----
const DEVICE_ID: usize = 0x008;
const DEVICE_FEATURES: usize = 0x010;
const DEVICE_FEATURES_SEL: usize = 0x014;
const DRIVER_FEATURES: usize = 0x020;
const DRIVER_FEATURES_SEL: usize = 0x024;
const QUEUE_SEL: usize = 0x030;
const QUEUE_NUM_MAX: usize = 0x034;
const QUEUE_NUM: usize = 0x038;
const QUEUE_READY: usize = 0x044;
const QUEUE_NOTIFY: usize = 0x050;
const INTERRUPT_STATUS: usize = 0x060;
const INTERRUPT_ACK: usize = 0x064;
const STATUS: usize = 0x070;
const QUEUE_DESC_LOW: usize = 0x080;
const QUEUE_DESC_HIGH: usize = 0x084;
const QUEUE_DRIVER_LOW: usize = 0x090;
const QUEUE_DRIVER_HIGH: usize = 0x094;
const QUEUE_DEVICE_LOW: usize = 0x0a0;
const QUEUE_DEVICE_HIGH: usize = 0x0a4;
const CONFIG: usize = 0x100;

const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;
const STATUS_FAILED: u32 = 128;

const QUEUE_SIZE: usize = 8;
const AVAIL_OFF: usize = 16 * QUEUE_SIZE;
const USED_OFF: usize = 2048;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

const BLK_T_IN: u32 = 0;
const BLK_T_OUT: u32 = 1;

// Wire protocol with whoever holds the other end of the channels.
const OP_INFO: u8 = 0;
const OP_READ: u8 = 1;
const OP_WRITE: u8 = 2;
const OP_SHUTDOWN: u8 = 3;

#[inline]
fn mmio() -> usize {
    unsafe { core::ptr::read_volatile(&raw const MMIO_VA) }
}

#[inline]
unsafe fn rd(off: usize) -> u32 {
    core::ptr::read_volatile((mmio() + off) as *const u32)
}

#[inline]
unsafe fn wr(off: usize, v: u32) {
    core::ptr::write_volatile((mmio() + off) as *mut u32, v)
}

struct Disk {
    /// Physical address of the DMA region, which is what the device understands.
    dma_phys: usize,
    capacity: u64,
    last_used: u16,
    avail_idx: u16,
    interrupts: u64,
    exhausted: u64,
    timeouts: u64,
}

/// Buffer layout inside the second DMA page.
const BUF_OFF: usize = PAGE;
const HDR_OFF: usize = BUF_OFF;
const DATA_OFF: usize = BUF_OFF + 64;
const STATUS_OFF: usize = BUF_OFF + 64 + SECTOR;

impl Disk {
    fn desc(&self, i: usize) -> *mut u8 {
        (DMA_VA + i * 16) as *mut u8
    }

    unsafe fn write_desc(&self, i: usize, addr: u64, len: u32, flags: u16, next: u16) {
        let d = self.desc(i);
        core::ptr::write_volatile(d as *mut u64, addr);
        core::ptr::write_volatile(d.add(8) as *mut u32, len);
        core::ptr::write_volatile(d.add(12) as *mut u16, flags);
        core::ptr::write_volatile(d.add(14) as *mut u16, next);
    }

    fn avail_idx_ptr(&self) -> *mut u16 {
        (DMA_VA + AVAIL_OFF + 2) as *mut u16
    }

    fn avail_ring(&self, i: usize) -> *mut u16 {
        (DMA_VA + AVAIL_OFF + 4 + i * 2) as *mut u16
    }

    fn used_idx(&self) -> u16 {
        unsafe { core::ptr::read_volatile((DMA_VA + USED_OFF + 2) as *const u16) }
    }

    /// Claim the device we were given and bring it up.
    fn attach() -> Option<Disk> {
        // The capability says which device; we only say where to put it.
        let offset = map_device(CAP_MMIO, MMIO_PAGE);
        if offset < 0 {
            say("  [blkdrv  ]", " cannot map the device registers");
            return None;
        }
        unsafe { core::ptr::write_volatile(&raw mut MMIO_VA, MMIO_PAGE + offset as usize) };
        let dma_phys = dma_map(CAP_DMA, DMA_VA);
        if dma_phys < 0 {
            say("  [blkdrv  ]", " cannot map a DMA region");
            return None;
        }
        let dma_phys = dma_phys as usize;

        unsafe {
            if rd(DEVICE_ID) != 2 {
                say("  [blkdrv  ]", " that is not a block device");
                return None;
            }

            wr(STATUS, 0);
            wr(STATUS, STATUS_ACKNOWLEDGE);
            wr(STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

            // Accept VIRTIO_F_VERSION_1 and nothing else.
            wr(DEVICE_FEATURES_SEL, 1);
            if rd(DEVICE_FEATURES) & 1 == 0 {
                wr(STATUS, STATUS_FAILED);
                say("  [blkdrv  ]", " device does not offer VIRTIO_F_VERSION_1");
                return None;
            }
            wr(DRIVER_FEATURES_SEL, 1);
            wr(DRIVER_FEATURES, 1);
            wr(DRIVER_FEATURES_SEL, 0);
            wr(DRIVER_FEATURES, 0);

            wr(STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
            if rd(STATUS) & STATUS_FEATURES_OK == 0 {
                wr(STATUS, STATUS_FAILED);
                return None;
            }

            let capacity = core::ptr::read_volatile((mmio() + CONFIG) as *const u64);

            core::ptr::write_bytes(DMA_VA as *mut u8, 0, 2 * PAGE);

            wr(QUEUE_SEL, 0);
            if rd(QUEUE_NUM_MAX) < QUEUE_SIZE as u32 {
                wr(STATUS, STATUS_FAILED);
                return None;
            }
            wr(QUEUE_NUM, QUEUE_SIZE as u32);
            wr(QUEUE_DESC_LOW, dma_phys as u32);
            wr(QUEUE_DESC_HIGH, (dma_phys >> 32) as u32);
            wr(QUEUE_DRIVER_LOW, (dma_phys + AVAIL_OFF) as u32);
            wr(QUEUE_DRIVER_HIGH, ((dma_phys + AVAIL_OFF) >> 32) as u32);
            wr(QUEUE_DEVICE_LOW, (dma_phys + USED_OFF) as u32);
            wr(QUEUE_DEVICE_HIGH, ((dma_phys + USED_OFF) >> 32) as u32);
            wr(QUEUE_READY, 1);

            wr(STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);

            Some(Disk { dma_phys, capacity, last_used: 0, avail_idx: 0, interrupts: 0, exhausted: 0, timeouts: 0 })
        }
    }

    /// One request, start to finish.
    fn request(&mut self, kind: u32, sector: u64, data: &mut [u8]) -> bool {
        unsafe {
            let hdr = (DMA_VA + HDR_OFF) as *mut u32;
            core::ptr::write_volatile(hdr, kind);
            core::ptr::write_volatile(hdr.add(1), 0);
            core::ptr::write_volatile((DMA_VA + HDR_OFF + 8) as *mut u64, sector);
            core::ptr::write_volatile((DMA_VA + STATUS_OFF) as *mut u8, 0xff);

            if kind == BLK_T_OUT {
                core::ptr::copy_nonoverlapping(data.as_ptr(), (DMA_VA + DATA_OFF) as *mut u8, SECTOR);
            }

            self.write_desc(0, (self.dma_phys + HDR_OFF) as u64, 16, DESC_F_NEXT, 1);
            self.write_desc(
                1,
                (self.dma_phys + DATA_OFF) as u64,
                SECTOR as u32,
                DESC_F_NEXT | if kind == BLK_T_IN { DESC_F_WRITE } else { 0 },
                2,
            );
            self.write_desc(2, (self.dma_phys + STATUS_OFF) as u64, 1, DESC_F_WRITE, 0);

            core::ptr::write_volatile(self.avail_ring(self.avail_idx as usize % QUEUE_SIZE), 0);
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            self.avail_idx = self.avail_idx.wrapping_add(1);
            core::ptr::write_volatile(self.avail_idx_ptr(), self.avail_idx);
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

            wr(QUEUE_NOTIFY, 0);
        }

        if !self.wait() {
            return false;
        }

        unsafe {
            if core::ptr::read_volatile((DMA_VA + STATUS_OFF) as *const u8) != 0 {
                return false;
            }
            if kind == BLK_T_IN {
                core::ptr::copy_nonoverlapping((DMA_VA + DATA_OFF) as *const u8, data.as_mut_ptr(), SECTOR);
            }
        }
        true
    }

    /// Wait for the device, on its interrupt.
    ///
    /// The kernel masks the line when it fires, because only this process can
    /// quiet the device — so acknowledging `INTERRUPT_STATUS` here is what lets
    /// the next one through. A timeout is passed because a driver that can
    /// block forever on a line that never asserts is a driver that wedges
    /// itself.
    fn wait(&mut self) -> bool {
        for _ in 0..8 {
            let used = self.used_idx();
            if used != self.last_used {
                self.last_used = used;
                return true;
            }
            let r = irq_wait(CAP_IRQ, 50);
            if r < 0 {
                self.timeouts += 1;
            }
            if r > 0 {
                self.interrupts = r as u64;
                unsafe {
                    let status = rd(INTERRUPT_STATUS);
                    if status != 0 {
                        wr(INTERRUPT_ACK, status);
                    }
                }
            }
        }
        let used = self.used_idx();
        if used != self.last_used {
            self.last_used = used;
            return true;
        }
        self.exhausted += 1;
        false
    }
}

fn le64(b: &[u8]) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[..8]);
    u64::from_le_bytes(v)
}

/// The driver process: attach, then answer requests until told to stop.
pub fn run() {
    let Some(mut disk) = Disk::attach() else {
        say("  [blkdrv  ]", " failed to attach; exiting");
        return;
    };
    Line::new()
        .s("  [blkdrv  ] attached in userspace: ")
        .d(disk.capacity as usize)
        .s(" sectors, dma at ")
        .x(disk.dma_phys)
        .nl();

    // `[op][tag:4][sector:8]`, then the data for a write.
    const HEADER: usize = 13;
    const REPLY_HEADER: usize = 5;
    let mut req = [0u8; HEADER + SECTOR];
    let mut rep = [0u8; REPLY_HEADER + SECTOR];

    loop {
        let n = match recv(CAP_REQ, &mut req) {
            Ok(n) => n,
            Err(_) => return,
        };
        if n < HEADER {
            continue;
        }
        let op = req[0];
        // Echoed back so the client can tell this reply from a late one.
        let tag = &req[1..5];
        let sector = le64(&req[5..13]);
        rep = [0u8; REPLY_HEADER + SECTOR];
        rep[1..5].copy_from_slice(tag);

        match op {
            OP_INFO => {
                rep[0] = 0;
                rep[REPLY_HEADER..REPLY_HEADER + 8].copy_from_slice(&disk.capacity.to_le_bytes());
                let _ = send(CAP_REP, &rep[..REPLY_HEADER + 8]);
            }
            OP_READ => {
                let mut data = [0u8; SECTOR];
                if disk.request(BLK_T_IN, sector, &mut data) {
                    rep[0] = 0;
                    rep[REPLY_HEADER..].copy_from_slice(&data);
                } else {
                    rep[0] = 1;
                }
                let _ = send(CAP_REP, &rep);
            }
            OP_WRITE => {
                let mut data = [0u8; SECTOR];
                data.copy_from_slice(&req[HEADER..HEADER + SECTOR]);
                rep[0] = if disk.request(BLK_T_OUT, sector, &mut data) { 0 } else { 1 };
                let _ = send(CAP_REP, &rep[..REPLY_HEADER]);
            }
            OP_SHUTDOWN => {
                Line::new()
                    .s("  [blkdrv  ] shutting down after ")
                    .d(disk.interrupts as usize)
                    .s(" device interrupts, ")
                    .d(disk.timeouts as usize)
                    .s(" irq timeouts, ")
                    .d(disk.exhausted as usize)
                    .s(" requests given up on")
                    .nl();
                rep[0] = 0;
                let _ = send(CAP_REP, &rep[..REPLY_HEADER]);
                return;
            }
            _ => {
                rep[0] = 1;
                let _ = send(CAP_REP, &rep[..REPLY_HEADER]);
            }
        }
    }
}
