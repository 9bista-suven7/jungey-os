//! virtio-mmio transport and a block driver on top of it.
//!
//! virtio is the right first storage target: it is what every VM offers, it is
//! specified rather than reverse-engineered, and its queue model — descriptor
//! chains, an available ring the driver writes, a used ring the device writes —
//! is the same shape as the shared-memory rings this OS wants for IPC and for
//! handing work to an accelerator. What is learned here is not thrown away when
//! the target becomes UFS.
//!
//! Spec: virtio 1.2, sections 4.2 (MMIO) and 5.2 (block device).

use crate::dtb::DeviceNode;
use crate::mm::{frames, phys_to_virt, PAGE_SIZE};
use crate::sync::SpinLock;
use crate::{gic, irq, sched, time};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, AtomicU64, Ordering};

// ---- MMIO register offsets (virtio 1.2 §4.2.2) ----------------------------

const MAGIC: usize = 0x000;
const VERSION: usize = 0x004;
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

const MAGIC_VALUE: u32 = 0x7472_6976; // "virt"
const DEVICE_ID_BLOCK: u32 = 2;

// Device status bits (§2.1)
const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;
const STATUS_FAILED: u32 = 128;

/// VIRTIO_F_VERSION_1: the only feature this driver requires, and the one that
/// says the device speaks the modern spec rather than the legacy layout.
const F_VERSION_1: u32 = 32;

// ---- split virtqueue (§2.7) ----------------------------------------------

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2; // device writes; driver reads

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// Queue depth. Small: this driver issues one request at a time, and a deep
/// queue would only hide that.
const QUEUE_SIZE: usize = 8;

/// Offsets of the three queue areas within one page. They may live apart under
/// the modern MMIO interface, so one page holds all three comfortably.
const AVAIL_OFF: usize = 16 * QUEUE_SIZE; // after the descriptor table
const USED_OFF: usize = 2048; // 4-byte aligned, clear of the avail ring

pub const SECTOR_SIZE: usize = 512;

// ---- block request (§5.2.6) ----------------------------------------------

const BLK_T_IN: u32 = 0; // read
const BLK_T_OUT: u32 = 1; // write

#[repr(C)]
struct BlkReqHeader {
    kind: u32,
    reserved: u32,
    sector: u64,
}

/// Wake token for threads waiting on block completion.
const BLK_TOKEN: u64 = 0xB10C_0000;

static IRQ_COUNT: AtomicU64 = AtomicU64::new(0);

pub struct VirtioBlk {
    base: usize,
    /// Physical base of the queue page; the device only ever sees physical.
    queue_phys: usize,
    queue_virt: usize,
    /// Bounce buffer: one sector, plus the request header and status byte.
    buf_phys: usize,
    buf_virt: usize,
    last_used: u16,
    avail_idx: u16,
    pub capacity_sectors: u64,
    pub irq: u32,
}

// Safety: guarded by the SpinLock below; the device is not shared otherwise.
unsafe impl Send for VirtioBlk {}

#[inline]
unsafe fn rd(base: usize, off: usize) -> u32 {
    read_volatile((base + off) as *const u32)
}

#[inline]
unsafe fn wr(base: usize, off: usize, v: u32) {
    write_volatile((base + off) as *mut u32, v)
}

impl VirtioBlk {
    fn desc(&self, i: usize) -> *mut Desc {
        (self.queue_virt + i * 16) as *mut Desc
    }

    fn avail_flags(&self) -> *mut u16 {
        (self.queue_virt + AVAIL_OFF) as *mut u16
    }

    fn avail_idx_ptr(&self) -> *mut u16 {
        (self.queue_virt + AVAIL_OFF + 2) as *mut u16
    }

    fn avail_ring(&self, i: usize) -> *mut u16 {
        (self.queue_virt + AVAIL_OFF + 4 + i * 2) as *mut u16
    }

    fn used_idx_ptr(&self) -> *const u16 {
        (self.queue_virt + USED_OFF + 2) as *const u16
    }

    /// Probe one virtio-mmio slot and attach if it is a block device.
    ///
    /// QEMU's `virt` board advertises 32 transport slots whether or not
    /// anything is plugged into them, so probing is not optional.
    pub fn probe(node: &DeviceNode) -> Option<VirtioBlk> {
        let base = phys_to_virt(node.base as usize);
        unsafe {
            if rd(base, MAGIC) != MAGIC_VALUE {
                return None;
            }
            if rd(base, DEVICE_ID) != DEVICE_ID_BLOCK {
                return None; // empty slot, or some other device
            }
            if rd(base, VERSION) != 2 {
                return None; // legacy transport: a different driver entirely
            }

            // Reset, then walk the initialisation sequence of §3.1.
            wr(base, STATUS, 0);
            wr(base, STATUS, STATUS_ACKNOWLEDGE);
            wr(base, STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

            // Accept exactly one feature. Anything else the device offers is
            // declined, which is always allowed and keeps the driver honest.
            wr(base, DEVICE_FEATURES_SEL, 1);
            let hi = rd(base, DEVICE_FEATURES);
            if hi & (1 << (F_VERSION_1 - 32)) == 0 {
                wr(base, STATUS, STATUS_FAILED);
                return None;
            }
            wr(base, DRIVER_FEATURES_SEL, 1);
            wr(base, DRIVER_FEATURES, 1 << (F_VERSION_1 - 32));
            wr(base, DRIVER_FEATURES_SEL, 0);
            wr(base, DRIVER_FEATURES, 0);

            wr(base, STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
            if rd(base, STATUS) & STATUS_FEATURES_OK == 0 {
                wr(base, STATUS, STATUS_FAILED);
                return None;
            }

            // Capacity is the first field of block config space, in sectors.
            let capacity_sectors = read_volatile((base + CONFIG) as *const u64);

            // One page for the queue, one for the request header, data and
            // status byte. Both must be contiguous: the device sees physical
            // addresses and does not walk our page tables.
            let queue_phys = frames::alloc_contiguous(1)?;
            let buf_phys = frames::alloc_contiguous(1)?;
            let queue_virt = phys_to_virt(queue_phys);
            let buf_virt = phys_to_virt(buf_phys);
            core::ptr::write_bytes(queue_virt as *mut u8, 0, PAGE_SIZE);
            core::ptr::write_bytes(buf_virt as *mut u8, 0, PAGE_SIZE);

            // Queue 0 is the only one a block device has.
            wr(base, QUEUE_SEL, 0);
            if rd(base, QUEUE_NUM_MAX) < QUEUE_SIZE as u32 {
                wr(base, STATUS, STATUS_FAILED);
                return None;
            }
            wr(base, QUEUE_NUM, QUEUE_SIZE as u32);
            wr(base, QUEUE_DESC_LOW, queue_phys as u32);
            wr(base, QUEUE_DESC_HIGH, (queue_phys >> 32) as u32);
            wr(base, QUEUE_DRIVER_LOW, (queue_phys + AVAIL_OFF) as u32);
            wr(base, QUEUE_DRIVER_HIGH, ((queue_phys + AVAIL_OFF) >> 32) as u32);
            wr(base, QUEUE_DEVICE_LOW, (queue_phys + USED_OFF) as u32);
            wr(base, QUEUE_DEVICE_HIGH, ((queue_phys + USED_OFF) >> 32) as u32);
            wr(base, QUEUE_READY, 1);

            wr(base, STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);

            Some(VirtioBlk {
                base,
                queue_phys,
                queue_virt,
                buf_phys,
                buf_virt,
                last_used: 0,
                avail_idx: 0,
                capacity_sectors,
                irq: node.irq,
            })
        }
    }

    /// Run one request to completion.
    ///
    /// Three descriptors, as the spec requires: a device-readable header, the
    /// data (readable for a write, writable for a read), and a one-byte status
    /// the device writes.
    fn request(&mut self, kind: u32, sector: u64, data: &mut [u8]) -> Result<(), &'static str> {
        if data.len() != SECTOR_SIZE {
            return Err("request must be exactly one sector");
        }

        // Layout inside the bounce page: header, data, status.
        let hdr_phys = self.buf_phys;
        let data_phys = self.buf_phys + 64;
        let status_phys = self.buf_phys + 64 + SECTOR_SIZE;
        let hdr = self.buf_virt as *mut BlkReqHeader;
        let data_virt = (self.buf_virt + 64) as *mut u8;
        let status_virt = (self.buf_virt + 64 + SECTOR_SIZE) as *mut u8;

        unsafe {
            (*hdr).kind = kind;
            (*hdr).reserved = 0;
            (*hdr).sector = sector;
            write_volatile(status_virt, 0xff); // anything but a status the device could write
            if kind == BLK_T_OUT {
                core::ptr::copy_nonoverlapping(data.as_ptr(), data_virt, SECTOR_SIZE);
            }

            *self.desc(0) = Desc {
                addr: hdr_phys as u64,
                len: 16,
                flags: DESC_F_NEXT,
                next: 1,
            };
            *self.desc(1) = Desc {
                addr: data_phys as u64,
                len: SECTOR_SIZE as u32,
                // A read has the device writing into our buffer; a write does not.
                flags: DESC_F_NEXT | if kind == BLK_T_IN { DESC_F_WRITE } else { 0 },
                next: 2,
            };
            *self.desc(2) = Desc {
                addr: status_phys as u64,
                len: 1,
                flags: DESC_F_WRITE,
                next: 0,
            };

            write_volatile(self.avail_flags(), 0);
            write_volatile(self.avail_ring(self.avail_idx as usize % QUEUE_SIZE), 0);

            // The device must see the descriptors before the index that
            // publishes them, and the ordering is not otherwise guaranteed.
            fence(Ordering::SeqCst);
            self.avail_idx = self.avail_idx.wrapping_add(1);
            write_volatile(self.avail_idx_ptr(), self.avail_idx);
            fence(Ordering::SeqCst);

            wr(self.base, QUEUE_NOTIFY, 0);
        }

        self.wait_for_completion()?;

        unsafe {
            let status = read_volatile(status_virt);
            if status != 0 {
                return Err("device reported an error");
            }
            if kind == BLK_T_IN {
                core::ptr::copy_nonoverlapping(data_virt, data.as_mut_ptr(), SECTOR_SIZE);
            }
        }
        Ok(())
    }

    /// Wait for the used ring to advance, yielding rather than spinning.
    ///
    /// The completion interrupt fires and is counted, but the wait does not
    /// depend on it: a driver that can only be woken by an interrupt hangs the
    /// machine when the interrupt does not arrive. Interrupt-driven completion
    /// arrives with stage 3d, where an IRQ becomes a message to a driver
    /// process and a lost one is that process's problem, not the kernel's.
    fn wait_for_completion(&mut self) -> Result<(), &'static str> {
        let deadline = time::ticks() + 200; // two seconds at 100 Hz
        loop {
            fence(Ordering::SeqCst);
            let used = unsafe { read_volatile(self.used_idx_ptr()) };
            if used != self.last_used {
                self.last_used = used;
                return Ok(());
            }
            if time::ticks() > deadline {
                return Err("device did not complete the request in time");
            }
            sched::yield_now();
        }
    }

    pub fn read_sector(&mut self, sector: u64, data: &mut [u8]) -> Result<(), &'static str> {
        self.request(BLK_T_IN, sector, data)
    }

    pub fn write_sector(&mut self, sector: u64, data: &[u8]) -> Result<(), &'static str> {
        let mut scratch = [0u8; SECTOR_SIZE];
        scratch.copy_from_slice(data);
        self.request(BLK_T_OUT, sector, &mut scratch)
    }

    /// Physical address of the queue, for reporting.
    pub fn queue_phys(&self) -> usize {
        self.queue_phys
    }
}

static DISK: SpinLock<Option<VirtioBlk>> = SpinLock::new(None);

/// Acknowledge a completion interrupt and wake anyone waiting.
fn blk_interrupt() {
    let mut d = DISK.lock();
    if let Some(dev) = d.as_mut() {
        unsafe {
            let status = rd(dev.base, INTERRUPT_STATUS);
            if status != 0 {
                wr(dev.base, INTERRUPT_ACK, status);
            }
        }
    }
    drop(d);
    IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
    sched::wake_all_on(BLK_TOKEN);
}

/// Walk every virtio-mmio slot the device tree describes and attach the first
/// block device found.
pub fn init(fdt: &crate::dtb::Fdt) -> Option<(u64, u32, usize)> {
    let mut nodes = [DeviceNode::default(); 32];
    let n = fdt.devices("virtio,mmio", &mut nodes);
    // QEMU fills its transport slots from the last one downwards and leaves the
    // rest reporting device id 0, so every slot is probed.
    crate::println!("  virtio     : probing {} mmio transports", n);

    for node in nodes[..n].iter() {
        if let Some(dev) = VirtioBlk::probe(node) {
            let info = (dev.capacity_sectors, dev.irq, dev.queue_phys());
            let irq = dev.irq;
            *DISK.lock() = Some(dev);
            if irq >= 32 {
                irq::register(irq, blk_interrupt);
                gic::enable_spi(irq);
            }
            return Some(info);
        }
    }
    None
}

pub fn read_sector(sector: u64, data: &mut [u8]) -> Result<(), &'static str> {
    let mut d = DISK.lock();
    d.as_mut().ok_or("no disk")?.read_sector(sector, data)
}

pub fn write_sector(sector: u64, data: &[u8]) -> Result<(), &'static str> {
    let mut d = DISK.lock();
    d.as_mut().ok_or("no disk")?.write_sector(sector, data)
}

/// Is a block device attached?
pub fn have_disk() -> bool {
    DISK.lock().is_some()
}

/// Size of the attached disk, in sectors.
pub fn capacity_sectors() -> u64 {
    DISK.lock().as_ref().map(|d| d.capacity_sectors).unwrap_or(0)
}

/// Completion interrupts taken since boot.
pub fn irq_count() -> u64 {
    IRQ_COUNT.load(Ordering::Relaxed)
}


