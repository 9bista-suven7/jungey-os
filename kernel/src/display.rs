//! Display client: builds frames and sends them to the display server.
//!
//! There is no rendering here and no knowledge of virtio-gpu. A frame is a
//! short list of drawing commands — clear, rect, text, present — sent down a
//! channel to a process that owns the hardware. The kernel decides *what* the
//! screen says; it has no idea *how* a pixel gets written, which is the same
//! arrangement as the block driver and for the same reason.

use crate::{ipc, sched, time};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

const OP_CLEAR: u8 = 0x01;
const OP_RECT: u8 = 0x02;
const OP_TEXT: u8 = 0x03;
const OP_PRESENT: u8 = 0x04;
const OP_SHUTDOWN: u8 = 0xff;

/// The server's receive buffer is one page; batches are flushed before they
/// reach it.
const BATCH_LIMIT: usize = 3600;
const REPLY_TIMEOUT_TICKS: u64 = 600;

static REQ_CHANNEL: AtomicUsize = AtomicUsize::new(usize::MAX);
static REP_CHANNEL: AtomicUsize = AtomicUsize::new(usize::MAX);
static FRAMES: AtomicUsize = AtomicUsize::new(0);

pub fn attach(request: usize, reply: usize) {
    REQ_CHANNEL.store(request, Ordering::Release);
    REP_CHANNEL.store(reply, Ordering::Release);
}

pub fn attached() -> bool {
    REQ_CHANNEL.load(Ordering::Acquire) != usize::MAX
}

pub fn frames() -> usize {
    FRAMES.load(Ordering::Relaxed)
}

fn round_trip(ops: Vec<u8>) -> bool {
    let req = REQ_CHANNEL.load(Ordering::Acquire);
    let rep = REP_CHANNEL.load(Ordering::Acquire);
    if req == usize::MAX || ops.is_empty() {
        return false;
    }
    if ipc::send(req, usize::MAX, ops).is_err() {
        return false;
    }
    let deadline = time::ticks() + REPLY_TIMEOUT_TICKS;
    loop {
        if ipc::try_recv(rep).is_some() {
            return true;
        }
        if time::ticks() > deadline {
            return false;
        }
        sched::yield_now();
    }
}

/// A frame under construction. Commands accumulate and are flushed in batches,
/// so a screen with more text on it than fits in one message still works.
pub struct Frame {
    ops: Vec<u8>,
}

impl Default for Frame {
    fn default() -> Self {
        Self::new()
    }
}

impl Frame {
    pub fn new() -> Frame {
        Frame { ops: Vec::with_capacity(BATCH_LIMIT) }
    }

    fn room_for(&mut self, n: usize) {
        if self.ops.len() + n > BATCH_LIMIT {
            let batch = core::mem::take(&mut self.ops);
            round_trip(batch);
        }
    }

    pub fn clear(&mut self, color: u32) -> &mut Self {
        self.room_for(5);
        self.ops.push(OP_CLEAR);
        self.ops.extend_from_slice(&color.to_le_bytes());
        self
    }

    pub fn rect(&mut self, x: u16, y: u16, w: u16, h: u16, color: u32) -> &mut Self {
        self.room_for(13);
        self.ops.push(OP_RECT);
        for v in [x, y, w, h] {
            self.ops.extend_from_slice(&v.to_be_bytes());
        }
        self.ops.extend_from_slice(&color.to_le_bytes());
        self
    }

    pub fn text(&mut self, x: u16, y: u16, scale: u8, color: u32, s: &str) -> &mut Self {
        let bytes = s.as_bytes();
        let len = bytes.len().min(200);
        self.room_for(11 + len);
        self.ops.push(OP_TEXT);
        self.ops.extend_from_slice(&x.to_be_bytes());
        self.ops.extend_from_slice(&y.to_be_bytes());
        self.ops.extend_from_slice(&color.to_le_bytes());
        self.ops.push(scale);
        self.ops.push(len as u8);
        self.ops.extend_from_slice(&bytes[..len]);
        self
    }

    /// Send everything and show the result.
    pub fn present(&mut self) -> bool {
        self.room_for(1);
        self.ops.push(OP_PRESENT);
        let batch = core::mem::take(&mut self.ops);
        let ok = round_trip(batch);
        if ok {
            FRAMES.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }
}

/// Ask the display server to exit.
pub fn shutdown() {
    round_trip(alloc::vec![OP_SHUTDOWN]);
}
