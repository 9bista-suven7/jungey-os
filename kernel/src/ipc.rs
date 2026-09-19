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

/// Take a message, or mark the caller blocked — atomically, with respect to any
/// sender.
///
/// The two-step `prepare_block` / check / `block` discipline is not enough on
/// its own: between releasing the channel and marking oneself blocked there is
/// still a window, and a sender landing in it wakes a thread that is not yet
/// waiting. The wake is lost and the receiver sleeps with its message already
/// queued — for as long as it takes someone to send a *second* one.
///
/// Marking blocked while still holding the channel closes it by construction. A
/// sender has to take the same lock to enqueue, so it either enqueues first —
/// and we see the message — or it finds us already blocked and wakes us.
pub fn recv_or_prepare(channel: usize, token: u64) -> Option<Message> {
    let mut chans = CHANNELS.lock();
    let ch = chans.get_mut(channel)?;
    if let Some(m) = ch.queue.pop_front() {
        ch.received += 1;
        return Some(m);
    }
    crate::sched::prepare_block(token);
    None
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
