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
//! Since stage 4 it is also the *display server*: it owns the framebuffer, it
//! keeps a retained command list for the background that the kernel draws, it
//! composites the windows `wm.rs` tracks on top of it, and it decides which
//! window a tap belongs to. Only the damaged rectangle is transferred to the
//! host, which is the difference between moving a window costing 1.8 MB and
//! costing a few tens of kilobytes.
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
use crate::wm;

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

/// The display server's protocol.
///
/// The first four draw the *background*: they are retained, so the background
/// survives a window moving over it and back without the kernel redrawing it.
const OP_CLEAR: u8 = 0x01;
const OP_RECT: u8 = 0x02;
const OP_TEXT: u8 = 0x03;
const OP_PRESENT: u8 = 0x04;
/// A finished gesture from the input driver: where, and whether it is down.
const OP_INPUT: u8 = 0x10;
/// Window operations, all of them keyed by (sender, window id).
const OP_WIN_CREATE: u8 = 0x20;
const OP_WIN_RAISE: u8 = 0x21;
const OP_WIN_MOVE: u8 = 0x22;
const OP_WIN_TEXT: u8 = 0x23;
const OP_WIN_FILL: u8 = 0x24;
const OP_WIN_RESET: u8 = 0x25;
/// The kernel introducing a client: which capability slot reaches which pid.
const OP_CLIENT: u8 = 0x30;
/// Composite and show, reporting what it cost.
const OP_COMPOSE: u8 = 0x31;
/// Print what the compositor has seen. The kernel asks; nobody else can,
/// because nobody else is the kernel.
const OP_STATS: u8 = 0x32;
const OP_SHUTDOWN: u8 = 0xff;

/// What a client is told when a tap lands on its window: window-relative, so
/// an application never learns where its window is on screen.
const EV_TAP: u8 = 0x40;

/// `ipc::send` records the sender, and the kernel sends as `usize::MAX`. A
/// process cannot claim that, because it does not choose the value.
const KERNEL: usize = usize::MAX;

/// The compositor's capability slots past the five every driver holds: one
/// send capability per client it may deliver events to.
const CAP_CLIENT_BASE: usize = 5;
const MAX_CLIENTS: usize = 4;

static mut WINDOWS: wm::Wm = wm::Wm::new();

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
    /// Pixels actually sent to the host, so "we only redrew the damage" is a
    /// number rather than a claim.
    pixels: u64,
    /// The smallest transfer so far. The background is redrawn whole on every
    /// frame the kernel sends, so the average says little; what a damage
    /// rectangle buys shows up in the cheapest frame, which is the one where
    /// only a window changed.
    smallest: u64,
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

    /// Push one rectangle of the guest framebuffer to the host, then show it.
    ///
    /// The whole screen is the special case, not the rule: `offset` is where
    /// the rectangle's first pixel lives in the backing store, so the device
    /// reads exactly the pixels that changed. A window moving 200 px across a
    /// 480x960 screen transfers about a twentieth of it.
    fn present_rect(&mut self, r: wm::Rect) -> bool {
        let r = r.clamp_to_screen();
        if r.w == 0 || r.h == 0 {
            return true;
        }
        let mut at = self.hdr(CMD_OFF, CMD_TRANSFER_TO_HOST_2D);
        at = self.put_u32(at, r.x as u32);
        at = self.put_u32(at, r.y as u32);
        at = self.put_u32(at, r.w as u32);
        at = self.put_u32(at, r.h as u32);
        at = self.put_u64(at, ((r.y * WIDTH + r.x) * 4) as u64);
        at = self.put_u32(at, RESOURCE_ID);
        at = self.put_u32(at, 0);
        if !self.submit(at - CMD_OFF) {
            return false;
        }

        let mut at = self.hdr(CMD_OFF, CMD_RESOURCE_FLUSH);
        at = self.put_u32(at, r.x as u32);
        at = self.put_u32(at, r.y as u32);
        at = self.put_u32(at, r.w as u32);
        at = self.put_u32(at, r.h as u32);
        at = self.put_u32(at, RESOURCE_ID);
        at = self.put_u32(at, 0);
        let ok = self.submit(at - CMD_OFF);
        if ok {
            self.frames += 1;
            self.pixels += r.area() as u64;
            self.smallest = self.smallest.min(r.area() as u64);
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

        let mut gpu =
            Gpu { dma_phys, last_used: 0, avail_idx: 0, frames: 0, pixels: 0, smallest: u64::MAX };
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
//
// Everything here writes through `px`, which clips. Clipping is not a
// nicety: compositing the damaged rectangle means replaying drawing commands
// that mostly fall outside it, and the cheapest correct way to handle that is
// to let them run and drop the pixels that miss.

static mut CLIP: wm::Rect = wm::Rect { x: 0, y: 0, w: WIDTH, h: HEIGHT };

fn set_clip(r: wm::Rect) {
    unsafe { core::ptr::write_volatile(&raw mut CLIP, r.clamp_to_screen()) };
}

#[inline]
fn clip() -> wm::Rect {
    unsafe { core::ptr::read_volatile(&raw const CLIP) }
}

#[inline]
fn px(x: usize, y: usize, color: u32) {
    let c = clip();
    if !c.contains(x, y) {
        return;
    }
    unsafe {
        core::ptr::write_volatile((DMA_VA + FB_OFF + (y * WIDTH + x) * 4) as *mut u32, color);
    }
}

fn fill(x: usize, y: usize, w: usize, h: usize, color: u32) {
    let c = clip();
    let x0 = x.max(c.x);
    let y0 = y.max(c.y);
    let x1 = (x + w).min(c.x + c.w).min(WIDTH);
    let y1 = (y + h).min(c.y + c.h).min(HEIGHT);
    for row in y0..y1 {
        let base = DMA_VA + FB_OFF + (row * WIDTH) * 4;
        for col in x0..x1 {
            unsafe { core::ptr::write_volatile((base + col * 4) as *mut u32, color) };
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

/// The window manager draws through this and knows nothing else about the
/// framebuffer.
struct Fb;

impl wm::Painter for Fb {
    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        fill(x, y, w, h, color)
    }
    fn text(&mut self, x: usize, y: usize, scale: usize, color: u32, bytes: &[u8]) {
        text(bytes, x, y, scale.max(1), color)
    }
}

fn be16(b: &[u8]) -> usize {
    ((b[0] as usize) << 8) | b[1] as usize
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

// ---- the background, retained ---------------------------------------------
//
// The kernel draws the background as a list of commands. Keeping the list —
// rather than only the pixels it produced — is what makes a partial redraw
// possible: when a window moves, the strip it uncovered has to be drawn again,
// and only the commands know what was under it.

const ROOT_CAPACITY: usize = 16384;
static mut ROOT: [u8; ROOT_CAPACITY] = [0; ROOT_CAPACITY];
static mut ROOT_LEN: usize = 0;

fn root_reset() {
    unsafe { core::ptr::write_volatile(&raw mut ROOT_LEN, 0) };
}

fn root_append(bytes: &[u8]) {
    unsafe {
        let len = core::ptr::read_volatile(&raw const ROOT_LEN);
        if len + bytes.len() > ROOT_CAPACITY {
            return; // a background too complex to retain simply stops growing
        }
        let dst = (&raw mut ROOT) as *mut u8;
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(len), bytes.len());
        core::ptr::write_volatile(&raw mut ROOT_LEN, len + bytes.len());
    }
}

/// Replay the retained background into the current clip.
fn root_paint() {
    let (buf, len) = unsafe {
        (
            core::slice::from_raw_parts((&raw const ROOT) as *const u8, ROOT_CAPACITY),
            core::ptr::read_volatile(&raw const ROOT_LEN),
        )
    };
    let mut i = 0;
    while i < len {
        match buf[i] {
            OP_CLEAR => {
                fill(0, 0, WIDTH, HEIGHT, le32(&buf[i + 1..]));
                i += 5;
            }
            OP_RECT => {
                fill(
                    be16(&buf[i + 1..]),
                    be16(&buf[i + 3..]),
                    be16(&buf[i + 5..]),
                    be16(&buf[i + 7..]),
                    le32(&buf[i + 9..]),
                );
                i += 13;
            }
            OP_TEXT => {
                let x = be16(&buf[i + 1..]);
                let y = be16(&buf[i + 3..]);
                let color = le32(&buf[i + 5..]);
                let scale = buf[i + 9] as usize;
                let n = buf[i + 10] as usize;
                text(&buf[i + 11..i + 11 + n], x, y, scale.max(1), color);
                i += 11 + n;
            }
            _ => break,
        }
    }
}

// ---- the display server ---------------------------------------------------

struct Clients {
    pid: [usize; MAX_CLIENTS],
    n: usize,
}

impl Clients {
    /// Which capability slot reaches `pid`, if any. A client the kernel never
    /// introduced is not reachable, so its taps go nowhere and are counted.
    fn slot(&self, pid: usize) -> Option<usize> {
        self.pid[..self.n].iter().position(|&p| p == pid).map(|i| CAP_CLIENT_BASE + i)
    }
}

/// Composite and show. Returns the fraction of the screen that was sent.
fn compose(gpu: &mut Gpu, w: &mut wm::Wm) -> usize {
    let damage = w.damage.clamp_to_screen();
    if damage.w == 0 || damage.h == 0 {
        return 0;
    }
    set_clip(damage);
    root_paint();
    w.paint(&mut Fb);
    set_clip(wm::Rect { x: 0, y: 0, w: WIDTH, h: HEIGHT });
    gpu.present_rect(damage);
    w.damage = wm::Rect::EMPTY;
    damage.area() * 100 / (WIDTH * HEIGHT)
}

/// The display server: background, windows, input routing, and the device.
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

    // The window list is a static, not a local: it is a few kilobytes, this
    // process has one of it, and the roles in this binary are all inlined into
    // one `_start`, so a large stack frame here is a large stack frame for
    // every process the system runs.
    let windows: &mut wm::Wm = unsafe { &mut *(&raw mut WINDOWS) };
    let mut clients = Clients { pid: [0; MAX_CLIENTS], n: 0 };

    fill(0, 0, WIDTH, HEIGHT, 0xff000000);
    gpu.present_rect(wm::Rect { x: 0, y: 0, w: WIDTH, h: HEIGHT });

    let mut msg = [0u8; 4096];
    loop {
        let (n, from) = match recv_from(CAP_REQ, &mut msg) {
            Ok(r) => r,
            Err(_) => return,
        };
        if n == 0 {
            continue; // the input driver's heartbeat: nothing to do
        }

        let mut i = 0;
        let mut shutdown = false;
        while i < n {
            match msg[i] {
                // ---- the background, which only the kernel may draw ----
                OP_CLEAR if from == KERNEL => {
                    root_reset();
                    root_append(&msg[i..i + 5]);
                    windows.dirty_all();
                    i += 5;
                }
                OP_RECT if from == KERNEL => {
                    root_append(&msg[i..i + 13]);
                    windows.dirty(wm::Rect {
                        x: be16(&msg[i + 1..]),
                        y: be16(&msg[i + 3..]),
                        w: be16(&msg[i + 5..]),
                        h: be16(&msg[i + 7..]),
                    });
                    i += 13;
                }
                OP_TEXT if from == KERNEL => {
                    let len = msg[i + 10] as usize;
                    root_append(&msg[i..i + 11 + len]);
                    let scale = msg[i + 9].max(1) as usize;
                    windows.dirty(wm::Rect {
                        x: be16(&msg[i + 1..]),
                        y: be16(&msg[i + 3..]),
                        w: len * 8 * scale,
                        h: 8 * scale,
                    });
                    i += 11 + len;
                }
                OP_PRESENT | OP_COMPOSE => {
                    compose(&mut gpu, windows);
                    i += 1;
                }

                // ---- input, from the driver that holds the device ----
                OP_INPUT => {
                    let x = be16(&msg[i + 1..]);
                    let y = be16(&msg[i + 3..]);
                    let down = msg[i + 5] != 0;
                    if down {
                        route_tap(&clients, windows, x, y);
                    }
                    i += 6;
                }

                // ---- windows: keyed by (sender, id), never by id alone ----
                OP_WIN_CREATE => {
                    let id = msg[i + 1];
                    let rect = wm::Rect {
                        x: be16(&msg[i + 2..]),
                        y: be16(&msg[i + 4..]),
                        w: be16(&msg[i + 6..]),
                        h: be16(&msg[i + 8..]),
                    };
                    let bg = le32(&msg[i + 10..]);
                    let tl = msg[i + 14] as usize;
                    windows.create(from, id, rect, bg, &msg[i + 15..i + 15 + tl]);
                    i += 15 + tl;
                }
                OP_WIN_RAISE => {
                    windows.raise(from, msg[i + 1]);
                    i += 2;
                }
                OP_WIN_MOVE => {
                    let (x, y) = (be16(&msg[i + 2..]), be16(&msg[i + 4..]));
                    windows.with(from, msg[i + 1], |w| {
                        w.rect.x = x;
                        w.rect.y = y;
                        w.rect = w.rect.clamp_to_screen();
                    });
                    i += 6;
                }
                OP_WIN_FILL => {
                    let cmd = wm::Cmd::Fill {
                        x: be16(&msg[i + 2..]),
                        y: be16(&msg[i + 4..]),
                        w: be16(&msg[i + 6..]),
                        h: be16(&msg[i + 8..]),
                        color: le32(&msg[i + 10..]),
                    };
                    windows.push(from, msg[i + 1], cmd);
                    i += 14;
                }
                // [op][win][x:2][y:2][color:4][scale][len][bytes]
                OP_WIN_TEXT => {
                    let raw = msg[i + 11] as usize;
                    let len = raw.min(wm::MAX_TEXT);
                    let mut bytes = [0u8; wm::MAX_TEXT];
                    bytes[..len].copy_from_slice(&msg[i + 12..i + 12 + len]);
                    let cmd = wm::Cmd::Text {
                        x: be16(&msg[i + 2..]),
                        y: be16(&msg[i + 4..]),
                        color: le32(&msg[i + 6..]),
                        scale: msg[i + 10].max(1) as usize,
                        len,
                        bytes,
                    };
                    windows.push(from, msg[i + 1], cmd);
                    i += 12 + raw;
                }
                OP_WIN_RESET => {
                    windows.reset(from, msg[i + 1]);
                    i += 2;
                }

                // ---- the kernel wiring the system together ----
                OP_CLIENT if from == KERNEL => {
                    if clients.n < MAX_CLIENTS {
                        clients.pid[clients.n] =
                            u32::from_le_bytes([msg[i + 1], msg[i + 2], msg[i + 3], msg[i + 4]])
                                as usize;
                        clients.n += 1;
                    }
                    i += 5;
                }
                OP_STATS if from == KERNEL => {
                    report(&gpu, windows);
                    i += 1;
                }
                OP_SHUTDOWN if from == KERNEL => {
                    shutdown = true;
                    break;
                }
                _ => break, // malformed, or a client reaching past its authority
            }
        }

        // Only the kernel is answered, and the answer carries the counters
        // the kernel needs to judge whether the session went the way it was
        // supposed to. An application learns nothing about anyone else.
        if from == KERNEL {
            let reply = [
                0u8,
                (gpu.frames & 0xff) as u8,
                windows.taps_routed as u8,
                windows.taps_on_nothing as u8,
                windows.rejected as u8,
                (windows.composes & 0xff) as u8,
                windows.open() as u8,
            ];
            let _ = send(CAP_REP, &reply);
        }
        if shutdown {
            let mut l = Line::new();
            l.s("  [gpudrv  ] shutting down after ").d(gpu.frames as usize).s(" frames").nl();
            return;
        }
    }
}

/// Deliver a tap to exactly one window, or to nobody.
fn route_tap(clients: &Clients, windows: &mut wm::Wm, x: usize, y: usize) {
    let Some((owner, id, rx, ry)) = windows.hit(x, y) else {
        windows.taps_on_nothing += 1;
        let mut l = Line::new();
        l.s("  [gpudrv  ] tap at ").d(x).s(",").d(y).s(" hit no window").nl();
        return;
    };
    let Some(slot) = clients.slot(owner) else {
        windows.taps_on_nothing += 1;
        return;
    };
    let ev = [EV_TAP, id, (rx >> 8) as u8, rx as u8, (ry >> 8) as u8, ry as u8, 1];
    if send(slot, &ev).is_ok() {
        windows.taps_routed += 1;
        let mut l = Line::new();
        l.s("  [gpudrv  ] tap at ")
            .d(x)
            .s(",")
            .d(y)
            .s(" -> pid ")
            .d(owner)
            .s(" window ")
            .d(id as usize)
            .s(" at ")
            .d(rx)
            .s(",")
            .d(ry)
            .nl();
    }
}

fn report(gpu: &Gpu, w: &wm::Wm) {
    let mut l = Line::new();
    l.s("  [gpudrv  ] ")
        .d(w.composes)
        .s(" composites, ")
        .d(gpu.frames as usize)
        .s(" transfers, ")
        .d((gpu.pixels / 1000) as usize)
        .s("k pixels sent of ")
        .d(gpu.frames as usize * WIDTH * HEIGHT / 1000)
        .s("k a full screen would have cost")
        .nl();
    if gpu.smallest != u64::MAX {
        let mut l = Line::new();
        l.s("  [gpudrv  ] smallest transfer ")
            .d(gpu.smallest as usize / 1000)
            .s("k pixels, ")
            .d((gpu.smallest as usize * 100) / (WIDTH * HEIGHT))
            .s("% of the screen — a tap redraws one window, not the display")
            .nl();
    }
    let mut l = Line::new();
    l.s("  [gpudrv  ] ")
        .d(w.taps_routed)
        .s(" taps routed, ")
        .d(w.taps_on_nothing)
        .s(" landed on nothing, ")
        .d(w.rejected)
        .s(" operations named a window the sender does not own")
        .nl();
}
