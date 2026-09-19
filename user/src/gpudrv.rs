//! virtio-gpu driver — in userspace, like the block driver.
//!
//! The second device this OS drives, and the point of writing it was to find
//! out whether the capability framework from stage 3d carries something other
//! than the device it was designed around. It does: this process holds the same
//! five capabilities — its device's registers, its device's interrupt, a DMA
//! region, and two channels — and has no way to reach anything else. A bug in
//! the display driver cannot corrupt the disk, because it cannot name the disk.
//!
//! What it implements is the 2D subset of virtio-gpu: create a host resource,
//! attach guest memory as its backing store, point a scanout at it, and then
//! transfer and flush whenever the pixels change. No 3D, no cursor plane.
//!
//! Spec: virtio 1.2, section 5.7.
//!
//! The virtqueue mechanics here are close kin to `blkdrv`'s. Two drivers is not
//! yet enough to know what the right shared abstraction is, and copying subtle
//! code is a real cost — this is a debt, noted rather than hidden, to be paid
//! when a third driver says what the shape should be.

use crate::font;
use crate::say;
use crate::sys::*;

const CAP_REQ: usize = 0;
const CAP_REP: usize = 1;
const CAP_MMIO: usize = 2;
const CAP_IRQ: usize = 3;
const CAP_DMA: usize = 4;

const MMIO_PAGE: usize = 0x1200_0000;
const DMA_VA: usize = 0x1300_0000;
const PAGE: usize = 4096;

/// Phone-shaped, and small enough that a frame is under two megabytes.
pub const WIDTH: usize = 480;
pub const HEIGHT: usize = 960;
const FB_BYTES: usize = WIDTH * HEIGHT * 4;

// ---- virtio-mmio registers (same transport as the block device) ----
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

// ---- virtio-gpu commands (spec 5.7.6.7) ----
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const RESP_OK_NODATA: u32 = 0x1100;

/// B8G8R8A8: a pixel is 0xAARRGGBB as a little-endian u32, which is what the
/// drawing code below assumes.
const FORMAT_B8G8R8A8: u32 = 1;
const RESOURCE_ID: u32 = 1;

// Where things live inside the DMA region.
const CMD_OFF: usize = PAGE;
const RESP_OFF: usize = PAGE + 512;
const FB_OFF: usize = 2 * PAGE;

/// Drawing commands the display server accepts. One message is one frame.
const OP_CLEAR: u8 = 0x01;
const OP_RECT: u8 = 0x02;
const OP_TEXT: u8 = 0x03;
const OP_PRESENT: u8 = 0x04;

static mut MMIO_VA: usize = 0;

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

struct Gpu {
    dma_phys: usize,
    last_used: u16,
    avail_idx: u16,
    frames: u64,
}

impl Gpu {
    unsafe fn write_desc(&self, i: usize, addr: u64, len: u32, flags: u16, next: u16) {
        let d = (DMA_VA + i * 16) as *mut u8;
        core::ptr::write_volatile(d as *mut u64, addr);
        core::ptr::write_volatile(d.add(8) as *mut u32, len);
        core::ptr::write_volatile(d.add(12) as *mut u16, flags);
        core::ptr::write_volatile(d.add(14) as *mut u16, next);
    }

    fn used_idx(&self) -> u16 {
        unsafe { core::ptr::read_volatile((DMA_VA + USED_OFF + 2) as *const u16) }
    }

    /// Write a control header at `at`, returning the next free offset.
    fn hdr(&self, at: usize, cmd: u32) -> usize {
        unsafe {
            let p = (DMA_VA + at) as *mut u8;
            core::ptr::write_bytes(p, 0, 24);
            core::ptr::write_volatile(p as *mut u32, cmd);
        }
        at + 24
    }

    fn put_u32(&self, at: usize, v: u32) -> usize {
        unsafe { core::ptr::write_volatile((DMA_VA + at) as *mut u32, v) };
        at + 4
    }

    fn put_u64(&self, at: usize, v: u64) -> usize {
        unsafe { core::ptr::write_volatile((DMA_VA + at) as *mut u64, v) };
        at + 8
    }

    /// Submit the command built at CMD_OFF and wait for the device's answer.
    fn submit(&mut self, len: usize) -> bool {
        unsafe {
            core::ptr::write_volatile((DMA_VA + RESP_OFF) as *mut u32, 0);
            self.write_desc(0, (self.dma_phys + CMD_OFF) as u64, len as u32, DESC_F_NEXT, 1);
            self.write_desc(1, (self.dma_phys + RESP_OFF) as u64, 64, DESC_F_WRITE, 0);

            core::ptr::write_volatile((DMA_VA + AVAIL_OFF + 4) as *mut u16, 0);
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            self.avail_idx = self.avail_idx.wrapping_add(1);
            core::ptr::write_volatile((DMA_VA + AVAIL_OFF + 2) as *mut u16, self.avail_idx);
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            wr(QUEUE_NOTIFY, 0);
        }

        // Same discipline as the block driver: the interrupt is used, but the
        // wait does not depend on it arriving.
        for _ in 0..16 {
            let used = self.used_idx();
            if used != self.last_used {
                self.last_used = used;
                let resp = unsafe { core::ptr::read_volatile((DMA_VA + RESP_OFF) as *const u32) };
                return resp == RESP_OK_NODATA;
            }
            if irq_wait(CAP_IRQ, 20) > 0 {
                unsafe {
                    let s = rd(INTERRUPT_STATUS);
                    if s != 0 {
                        wr(INTERRUPT_ACK, s);
                    }
                }
            }
        }
        false
    }

    fn create_resource(&mut self) -> bool {
        let mut at = self.hdr(CMD_OFF, CMD_RESOURCE_CREATE_2D);
        at = self.put_u32(at, RESOURCE_ID);
        at = self.put_u32(at, FORMAT_B8G8R8A8);
        at = self.put_u32(at, WIDTH as u32);
        at = self.put_u32(at, HEIGHT as u32);
        self.submit(at - CMD_OFF)
    }

    /// Tell the device which guest memory holds the pixels. One entry, because
    /// the framebuffer came from a single contiguous DMA region.
    fn attach_backing(&mut self) -> bool {
        let mut at = self.hdr(CMD_OFF, CMD_RESOURCE_ATTACH_BACKING);
        at = self.put_u32(at, RESOURCE_ID);
        at = self.put_u32(at, 1); // one entry
        at = self.put_u64(at, (self.dma_phys + FB_OFF) as u64);
        at = self.put_u32(at, FB_BYTES as u32);
        at = self.put_u32(at, 0); // padding
        self.submit(at - CMD_OFF)
    }

    fn set_scanout(&mut self) -> bool {
        let mut at = self.hdr(CMD_OFF, CMD_SET_SCANOUT);
        at = self.put_u32(at, 0); // rect x
        at = self.put_u32(at, 0); // rect y
        at = self.put_u32(at, WIDTH as u32);
        at = self.put_u32(at, HEIGHT as u32);
        at = self.put_u32(at, 0); // scanout id
        at = self.put_u32(at, RESOURCE_ID);
        self.submit(at - CMD_OFF)
    }

    /// Push the guest framebuffer to the host resource, then show it.
    fn present(&mut self) -> bool {
        let mut at = self.hdr(CMD_OFF, CMD_TRANSFER_TO_HOST_2D);
        at = self.put_u32(at, 0);
        at = self.put_u32(at, 0);
        at = self.put_u32(at, WIDTH as u32);
        at = self.put_u32(at, HEIGHT as u32);
        at = self.put_u64(at, 0); // offset into the resource
        at = self.put_u32(at, RESOURCE_ID);
        at = self.put_u32(at, 0);
        if !self.submit(at - CMD_OFF) {
            return false;
        }

        let mut at = self.hdr(CMD_OFF, CMD_RESOURCE_FLUSH);
        at = self.put_u32(at, 0);
        at = self.put_u32(at, 0);
        at = self.put_u32(at, WIDTH as u32);
        at = self.put_u32(at, HEIGHT as u32);
        at = self.put_u32(at, RESOURCE_ID);
        at = self.put_u32(at, 0);
        let ok = self.submit(at - CMD_OFF);
        if ok {
            self.frames += 1;
        }
        ok
    }

    fn attach() -> Option<Gpu> {
        let offset = map_device(CAP_MMIO, MMIO_PAGE);
        if offset < 0 {
            say("  [gpudrv  ]", " cannot map the device registers");
            return None;
        }
        unsafe { core::ptr::write_volatile(&raw mut MMIO_VA, MMIO_PAGE + offset as usize) };

        let dma_phys = dma_map(CAP_DMA, DMA_VA);
        if dma_phys < 0 {
            say("  [gpudrv  ]", " cannot map a DMA region");
            return None;
        }
        let dma_phys = dma_phys as usize;

        unsafe {
            if rd(DEVICE_ID) != 16 {
                say("  [gpudrv  ]", " that is not a gpu");
                return None;
            }
            wr(STATUS, 0);
            wr(STATUS, STATUS_ACKNOWLEDGE);
            wr(STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

            wr(DEVICE_FEATURES_SEL, 1);
            if rd(DEVICE_FEATURES) & 1 == 0 {
                wr(STATUS, STATUS_FAILED);
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

            core::ptr::write_bytes(DMA_VA as *mut u8, 0, 2 * PAGE);

            wr(QUEUE_SEL, 0); // control queue
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
        }

        let mut gpu = Gpu { dma_phys, last_used: 0, avail_idx: 0, frames: 0 };
        if !gpu.create_resource() {
            say("  [gpudrv  ]", " the device refused to create a resource");
            return None;
        }
        if !gpu.attach_backing() {
            say("  [gpudrv  ]", " the device refused our framebuffer");
            return None;
        }
        if !gpu.set_scanout() {
            say("  [gpudrv  ]", " the device refused to scan out");
            return None;
        }
        Some(gpu)
    }
}

// ---- drawing -------------------------------------------------------------

#[inline]
fn px(x: usize, y: usize, color: u32) {
    if x >= WIDTH || y >= HEIGHT {
        return;
    }
    unsafe {
        core::ptr::write_volatile((DMA_VA + FB_OFF + (y * WIDTH + x) * 4) as *mut u32, color);
    }
}

fn clear(color: u32) {
    for i in 0..WIDTH * HEIGHT {
        unsafe { core::ptr::write_volatile((DMA_VA + FB_OFF + i * 4) as *mut u32, color) };
    }
}

fn rect(x: usize, y: usize, w: usize, h: usize, color: u32) {
    for dy in 0..h {
        for dx in 0..w {
            px(x + dx, y + dy, color);
        }
    }
}

/// Draw one character. Lowercase is folded to uppercase; anything without a
/// glyph is skipped rather than drawn as a box.
fn glyph(ch: u8, x: usize, y: usize, scale: usize, color: u32) {
    let c = if ch.is_ascii_lowercase() { ch - 32 } else { ch };
    if c < font::FIRST || c > font::LAST {
        return;
    }
    let bits = &font::GLYPHS[(c - font::FIRST) as usize];
    for (row, byte) in bits.iter().enumerate() {
        for col in 0..8 {
            if byte & (0x80 >> col) != 0 {
                for sy in 0..scale {
                    for sx in 0..scale {
                        px(x + col * scale + sx, y + row * scale + sy, color);
                    }
                }
            }
        }
    }
}

fn text(s: &[u8], x: usize, y: usize, scale: usize, color: u32) {
    for (i, &ch) in s.iter().enumerate() {
        glyph(ch, x + i * 8 * scale, y, scale, color);
    }
}

fn be16(b: &[u8]) -> usize {
    ((b[0] as usize) << 8) | b[1] as usize
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// The display server: take a frame's worth of drawing commands, render, show.
pub fn run() {
    let Some(mut gpu) = Gpu::attach() else {
        say("  [gpudrv  ]", " failed to attach; exiting");
        return;
    };
    let mut l = Line::new();
    l.s("  [gpudrv  ] attached in userspace: ")
        .d(WIDTH)
        .s("x")
        .d(HEIGHT)
        .s(", framebuffer at ")
        .x(gpu.dma_phys + FB_OFF)
        .nl();

    clear(0xff000000);
    gpu.present();

    let mut msg = [0u8; 4096];
    loop {
        let n = match recv(CAP_REQ, &mut msg) {
            Ok(n) => n,
            Err(_) => return,
        };

        let mut i = 0;
        let mut shutdown = false;
        while i < n {
            match msg[i] {
                OP_CLEAR => {
                    clear(le32(&msg[i + 1..]));
                    i += 5;
                }
                OP_RECT => {
                    rect(
                        be16(&msg[i + 1..]),
                        be16(&msg[i + 3..]),
                        be16(&msg[i + 5..]),
                        be16(&msg[i + 7..]),
                        le32(&msg[i + 9..]),
                    );
                    i += 13;
                }
                OP_TEXT => {
                    let x = be16(&msg[i + 1..]);
                    let y = be16(&msg[i + 3..]);
                    let color = le32(&msg[i + 5..]);
                    let scale = msg[i + 9] as usize;
                    let len = msg[i + 10] as usize;
                    text(&msg[i + 11..i + 11 + len], x, y, scale.max(1), color);
                    i += 11 + len;
                }
                OP_PRESENT => {
                    gpu.present();
                    i += 1;
                }
                0xff => {
                    shutdown = true;
                    break;
                }
                _ => break, // malformed: stop rather than guess
            }
        }

        let reply = [0u8, (gpu.frames & 0xff) as u8];
        let _ = send(CAP_REP, &reply);
        if shutdown {
            let mut l = Line::new();
            l.s("  [gpudrv  ] shutting down after ").d(gpu.frames as usize).s(" frames").nl();
            return;
        }
    }
}
