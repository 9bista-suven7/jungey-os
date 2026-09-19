//! virtio-input driver — the third device, and the first one that talks *to*
//! the system rather than being talked to.
//!
//! Its whole authority is four capabilities: its device's registers, its
//! device's interrupt, a DMA region, and one channel it may send on. It cannot
//! receive on that channel, so it can emit events and nothing else — an input
//! driver that could read the channel it feeds could read the compositor's
//! traffic, and there is no reason for it to be able to.
//!
//! The device is a tablet: an absolute pointer, which is what a touchscreen
//! looks like to software. A mouse would report deltas and need the compositor
//! to keep the cursor position; an absolute device reports where the finger is,
//! which is both simpler and what a phone actually has.
//!
//! Spec: virtio 1.2, section 5.8.

use crate::say;
use crate::sys::*;

const CAP_OUT: usize = 0; // send-only: events go up, nothing comes back
const CAP_MMIO: usize = 1;
const CAP_IRQ: usize = 2;
const CAP_DMA: usize = 3;

const MMIO_PAGE: usize = 0x1400_0000;
const DMA_VA: usize = 0x1500_0000;
const PAGE: usize = 4096;

// ---- virtio-mmio registers, the same transport as the other two drivers ----
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
/// Device-specific configuration space begins here on virtio-mmio.
const CONFIG: usize = 0x100;

const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;
const STATUS_FAILED: u32 = 128;

/// One event is eight bytes, so a queue of 64 is half a kilobyte and holds a
/// burst of movement without dropping any.
const QUEUE_SIZE: usize = 64;
const AVAIL_OFF: usize = 16 * QUEUE_SIZE;
const USED_OFF: usize = 2048;
const DESC_F_WRITE: u16 = 2;
/// The event buffers, one per descriptor, in the page after the rings.
const BUF_OFF: usize = PAGE;
const EVENT_SIZE: usize = 8;

// ---- virtio-input config selects (spec 5.8.4) ----
const CFG_ID_NAME: u8 = 0x01;
const CFG_ABS_INFO: u8 = 0x12;

// ---- Linux input event codes, which virtio-input uses verbatim ----
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_TOUCH: u16 = 0x14a;

/// The message this driver emits. One byte of opcode, then where and whether
/// the finger is down. The compositor does not know a virtqueue exists.
const OP_INPUT: u8 = 0x10;

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

#[inline]
unsafe fn rd8(off: usize) -> u8 {
    core::ptr::read_volatile((mmio() + off) as *const u8)
}

#[inline]
unsafe fn wr8(off: usize, v: u8) {
    core::ptr::write_volatile((mmio() + off) as *mut u8, v)
}

/// The range the device reports for one axis. A tablet is free to use any
/// range it likes — QEMU uses 0..32767 — so the driver asks rather than
/// assuming, and the compositor is handed screen coordinates.
struct Axis {
    min: i32,
    max: i32,
}

impl Axis {
    fn to_screen(&self, v: i32, extent: usize) -> usize {
        let span = (self.max - self.min).max(1) as i64;
        let pos = (v - self.min).clamp(0, span as i32) as i64;
        ((pos * (extent as i64 - 1)) / span) as usize
    }
}

struct Input {
    dma_phys: usize,
    last_used: u16,
    avail_idx: u16,
    x_axis: Axis,
    y_axis: Axis,
    events: u64,
}

impl Input {
    unsafe fn write_desc(&self, i: usize, addr: u64, len: u32, flags: u16) {
        let d = (DMA_VA + i * 16) as *mut u8;
        core::ptr::write_volatile(d as *mut u64, addr);
        core::ptr::write_volatile(d.add(8) as *mut u32, len);
        core::ptr::write_volatile(d.add(12) as *mut u16, flags);
        core::ptr::write_volatile(d.add(14) as *mut u16, 0);
    }

    fn used_idx(&self) -> u16 {
        unsafe { core::ptr::read_volatile((DMA_VA + USED_OFF + 2) as *const u16) }
    }

    /// Offer descriptor `i` back to the device.
    fn offer(&mut self, i: usize) {
        unsafe {
            let slot = (self.avail_idx as usize) % QUEUE_SIZE;
            core::ptr::write_volatile((DMA_VA + AVAIL_OFF + 4 + slot * 2) as *mut u16, i as u16);
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            self.avail_idx = self.avail_idx.wrapping_add(1);
            core::ptr::write_volatile((DMA_VA + AVAIL_OFF + 2) as *mut u16, self.avail_idx);
        }
    }

    /// Read one config field into `out`, returning how many bytes the device
    /// says it holds. Selecting a field the device does not have returns 0,
    /// which is how you ask "do you have an X axis?".
    fn config(&self, select: u8, subsel: u8, out: &mut [u8]) -> usize {
        unsafe {
            wr8(CONFIG, select);
            wr8(CONFIG + 1, subsel);
            let size = rd8(CONFIG + 2) as usize;
            let n = size.min(out.len());
            for (i, b) in out[..n].iter_mut().enumerate() {
                *b = rd8(CONFIG + 8 + i);
            }
            size
        }
    }

    fn axis(&self, code: u8) -> Axis {
        let mut buf = [0u8; 20];
        if self.config(CFG_ABS_INFO, code, &mut buf) >= 8 {
            let min = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            let max = i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
            if max > min {
                return Axis { min, max };
            }
        }
        // No absolute axis: the device is a relative pointer or a keyboard.
        Axis { min: 0, max: 0 }
    }

    fn attach() -> Option<Input> {
        let offset = map_device(CAP_MMIO, MMIO_PAGE);
        if offset < 0 {
            say("  [inputdrv]", " cannot map the device registers");
            return None;
        }
        unsafe { core::ptr::write_volatile(&raw mut MMIO_VA, MMIO_PAGE + offset as usize) };

        let dma_phys = dma_map(CAP_DMA, DMA_VA);
        if dma_phys < 0 {
            say("  [inputdrv]", " cannot map a DMA region");
            return None;
        }
        let dma_phys = dma_phys as usize;

        unsafe {
            if rd(DEVICE_ID) != 18 {
                say("  [inputdrv]", " that is not an input device");
                return None;
            }
            wr(STATUS, 0);
            wr(STATUS, STATUS_ACKNOWLEDGE);
            wr(STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

            // VIRTIO_F_VERSION_1 and nothing else: this driver has no use for
            // any feature the device might offer.
            wr(DEVICE_FEATURES_SEL, 1);
            if rd(DEVICE_FEATURES) & 1 == 0 {
                wr(STATUS, STATUS_FAILED);
                say("  [inputdrv]", " legacy device, refusing");
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

            wr(QUEUE_SEL, 0); // eventq
            if rd(QUEUE_NUM_MAX) < QUEUE_SIZE as u32 {
                wr(STATUS, STATUS_FAILED);
                say("  [inputdrv]", " the event queue is too short");
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
        }

        let mut dev = Input {
            dma_phys,
            last_used: 0,
            avail_idx: 0,
            x_axis: Axis { min: 0, max: 0 },
            y_axis: Axis { min: 0, max: 0 },
            events: 0,
        };
        dev.x_axis = dev.axis(ABS_X as u8);
        dev.y_axis = dev.axis(ABS_Y as u8);

        // Every buffer is offered before the device is told to start, so no
        // event can arrive with nowhere to go.
        for i in 0..QUEUE_SIZE {
            unsafe {
                dev.write_desc(
                    i,
                    (dma_phys + BUF_OFF + i * EVENT_SIZE) as u64,
                    EVENT_SIZE as u32,
                    DESC_F_WRITE,
                );
            }
            dev.offer(i);
        }
        unsafe {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            wr(STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
            wr(QUEUE_NOTIFY, 0);
        }
        Some(dev)
    }

    /// Read one completed event, or None if the device has produced nothing.
    fn next_event(&mut self) -> Option<(u16, u16, u32)> {
        if self.used_idx() == self.last_used {
            return None;
        }
        let slot = (self.last_used as usize) % QUEUE_SIZE;
        let id = unsafe {
            core::ptr::read_volatile((DMA_VA + USED_OFF + 4 + slot * 8) as *const u32) as usize
        };
        let id = id % QUEUE_SIZE;
        let at = DMA_VA + BUF_OFF + id * EVENT_SIZE;
        let ev = unsafe {
            (
                core::ptr::read_volatile(at as *const u16),
                core::ptr::read_volatile((at + 2) as *const u16),
                core::ptr::read_volatile((at + 4) as *const u32),
            )
        };
        self.last_used = self.last_used.wrapping_add(1);
        self.offer(id);
        unsafe { wr(QUEUE_NOTIFY, 0) };
        self.events += 1;
        Some(ev)
    }
}

/// Drain the device and forward finished gestures.
///
/// virtio-input reports one field per event and ends a group with EV_SYN, so a
/// tap arrives as "x is here", "y is here", "the button is down", "that is all
/// I have to say". Forwarding each field separately would make the compositor
/// reassemble them; forwarding on SYN means the compositor only ever sees
/// complete, consistent positions.
pub fn run() {
    let Some(mut dev) = Input::attach() else {
        say("  [inputdrv]", " failed to attach; exiting");
        return;
    };
    // The device names itself in its configuration space. Printing it is how
    // you find out the driver is reading config correctly before trusting the
    // axis ranges it read the same way.
    let mut name = [0u8; 40];
    let len = dev.config(CFG_ID_NAME, 0, &mut name).min(name.len());
    let mut l = Line::new();
    l.s("  [inputdrv] attached in userspace: \"")
        .s(core::str::from_utf8(&name[..len]).unwrap_or("?"))
        .s("\", dma at ")
        .x(dev.dma_phys)
        .nl();
    let mut l = Line::new();
    l.s("  [inputdrv] absolute pointer, x ")
        .d(dev.x_axis.min as usize)
        .s("..")
        .d(dev.x_axis.max as usize)
        .s(", y ")
        .d(dev.y_axis.min as usize)
        .s("..")
        .d(dev.y_axis.max as usize)
        .s(" -> ")
        .d(crate::gpudrv::WIDTH)
        .s("x")
        .d(crate::gpudrv::HEIGHT)
        .nl();

    let (mut x, mut y, mut down) = (0usize, 0usize, false);
    let mut sent = 0usize;
    let mut dirty = false;

    loop {
        while let Some((kind, code, value)) = dev.next_event() {
            match kind {
                EV_ABS if code == ABS_X => {
                    x = dev.x_axis.to_screen(value as i32, crate::gpudrv::WIDTH);
                    dirty = true;
                }
                EV_ABS if code == ABS_Y => {
                    y = dev.y_axis.to_screen(value as i32, crate::gpudrv::HEIGHT);
                    dirty = true;
                }
                EV_KEY if code == BTN_LEFT || code == BTN_TOUCH => {
                    down = value != 0;
                    dirty = true;
                }
                EV_SYN if dirty => {
                    let msg = [
                        OP_INPUT,
                        (x >> 8) as u8,
                        x as u8,
                        (y >> 8) as u8,
                        y as u8,
                        down as u8,
                    ];
                    // A failed send means the capability was revoked, which is
                    // how this process is told the session is over. There is no
                    // shutdown message, because it cannot receive one.
                    if send(CAP_OUT, &msg).is_err() {
                        let mut l = Line::new();
                        l.s("  [inputdrv] channel closed after ")
                            .d(dev.events as usize)
                            .s(" device events, ")
                            .d(sent)
                            .s(" gestures forwarded")
                            .nl();
                        return;
                    }
                    sent += 1;
                    dirty = false;
                }
                _ => {}
            }
        }

        // Block on the interrupt rather than spinning. The timeout is what
        // makes revocation noticeable: with no events at all this wakes,
        // sends nothing, and goes back to waiting.
        if irq_wait(CAP_IRQ, 20) > 0 {
            unsafe {
                let s = rd(INTERRUPT_STATUS);
                if s != 0 {
                    wr(INTERRUPT_ACK, s);
                }
            }
        } else if send(CAP_OUT, &[0u8; 0]).is_err() {
            let mut l = Line::new();
            l.s("  [inputdrv] channel closed after ")
                .d(dev.events as usize)
                .s(" device events, ")
                .d(sent)
                .s(" gestures forwarded")
                .nl();
            return;
        }
    }
}
