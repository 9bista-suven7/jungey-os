//! Channels: asynchronous, queued message passing between processes.
//!
//! A channel has no name a process can look up. The only way to reach one is to
//! hold a capability to it, which is why `send` and `recv` here take a channel
//! index the *kernel* extracted from a capability, never one a process
//! supplied.
//!
//! Stage 3 replaces the copy with shared-memory rings and zero-copy handoff of
//! memory objects; the authority model does not change.

use crate::sync::SpinLock;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

pub struct Message {
    pub from: usize,
    pub bytes: Vec<u8>,
}

struct Channel {
    queue: VecDeque<Message>,
    sent: u64,
    received: u64,
}

static CHANNELS: SpinLock<Vec<Channel>> = SpinLock::new(Vec::new());

/// Create a channel and return its index.
pub fn create() -> usize {
    let mut c = CHANNELS.lock();
    c.push(Channel { queue: VecDeque::new(), sent: 0, received: 0 });
    c.len() - 1
}

pub fn send(channel: usize, from: usize, bytes: Vec<u8>) -> Result<usize, &'static str> {
    let len = bytes.len();
    let mut chans = CHANNELS.lock();
    let ch = chans.get_mut(channel).ok_or("no such channel")?;
    ch.queue.push_back(Message { from, bytes });
    ch.sent += 1;
    drop(chans);
    // Anyone waiting on this channel can make progress now.
    crate::sched::wake_all_on(channel as u64);
    Ok(len)
}

pub fn try_recv(channel: usize) -> Option<Message> {
    let mut chans = CHANNELS.lock();
    let ch = chans.get_mut(channel)?;
    let m = ch.queue.pop_front();
    if m.is_some() {
        ch.received += 1;
    }
    m
}

/// (messages sent, messages received, still queued)
pub fn stats(channel: usize) -> (u64, u64, usize) {
    let chans = CHANNELS.lock();
    match chans.get(channel) {
        Some(ch) => (ch.sent, ch.received, ch.queue.len()),
        None => (0, 0, 0),
    }
}
