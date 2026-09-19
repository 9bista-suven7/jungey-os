//! The window manager: what is on the screen, who owns it, and who a tap
//! belongs to.
//!
//! This is policy, and it lives in userspace with the driver rather than in
//! the kernel — the kernel has no idea the screen has windows on it. It is in
//! the *same process* as the driver, though, and that is a compromise worth
//! naming: a compositor in its own process would have to be handed each frame,
//! and a frame here is 1.8 MB. Until processes can share a buffer rather than
//! copy a message, splitting them would cost 1.8 MB of copying per frame to buy
//! an isolation boundary between two pieces of code that trust each other
//! anyway. The split is real work and it is stage 4 proper; this is the part
//! that can be built honestly today.
//!
//! Two properties are worth stating because they are tested:
//!
//! - **A window belongs to the process that created it.** Windows are keyed by
//!   (owner, id), where the owner is the pid the *kernel* recorded at `send`
//!   time, not a field in the message. An application that guesses another's
//!   window id reaches its own window of that id, or nothing.
//! - **A tap goes to exactly one window**: the topmost one containing the
//!   point. Not the one that asked last, not all of them.

use crate::gpudrv::{HEIGHT, WIDTH};

pub const MAX_WINDOWS: usize = 8;
pub const MAX_CMDS: usize = 24;
pub const MAX_TEXT: usize = 40;
const TITLE_BAR: usize = 22;

#[derive(Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    pub const EMPTY: Rect = Rect { x: 0, y: 0, w: 0, h: 0 };

    pub fn contains(&self, px: usize, py: usize) -> bool {
        self.w != 0 && px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }

    pub fn area(&self) -> usize {
        self.w * self.h
    }

    /// The smallest rectangle covering both. Damage is tracked as one box
    /// rather than a list: it over-reports when two changes are far apart, and
    /// it is honest about that in the numbers it prints.
    pub fn union(&self, o: &Rect) -> Rect {
        if self.w == 0 {
            return *o;
        }
        if o.w == 0 {
            return *self;
        }
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = (self.x + self.w).max(o.x + o.w);
        let y1 = (self.y + self.h).max(o.y + o.h);
        Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }

    pub fn clamp_to_screen(&self) -> Rect {
        let x = self.x.min(WIDTH);
        let y = self.y.min(HEIGHT);
        Rect {
            x,
            y,
            w: (self.x + self.w).min(WIDTH).saturating_sub(x),
            h: (self.y + self.h).min(HEIGHT).saturating_sub(y),
        }
    }
}

#[derive(Clone, Copy)]
pub enum Cmd {
    None,
    Fill { x: usize, y: usize, w: usize, h: usize, color: u32 },
    Text { x: usize, y: usize, scale: usize, color: u32, len: usize, bytes: [u8; MAX_TEXT] },
}

#[derive(Clone, Copy)]
pub struct Window {
    pub used: bool,
    pub owner: usize,
    pub id: u8,
    pub rect: Rect,
    pub bg: u32,
    pub z: usize,
    pub title: [u8; 24],
    pub title_len: usize,
    pub cmds: [Cmd; MAX_CMDS],
    pub ncmd: usize,
}

impl Window {
    const BLANK: Window = Window {
        used: false,
        owner: 0,
        id: 0,
        rect: Rect::EMPTY,
        bg: 0,
        z: 0,
        title: [0; 24],
        title_len: 0,
        cmds: [Cmd::None; MAX_CMDS],
        ncmd: 0,
    };
}

pub struct Wm {
    pub windows: [Window; MAX_WINDOWS],
    pub next_z: usize,
    pub damage: Rect,
    /// Taps that landed on no window at all. A compositor that silently
    /// delivered those somewhere would be much harder to trust.
    pub taps_on_nothing: usize,
    pub taps_routed: usize,
    /// Operations naming a window the sender does not own.
    pub rejected: usize,
    pub composes: usize,
}

impl Wm {
    pub const fn new() -> Wm {
        Wm {
            windows: [Window::BLANK; MAX_WINDOWS],
            next_z: 1,
            damage: Rect::EMPTY,
            taps_on_nothing: 0,
            taps_routed: 0,
            rejected: 0,
            composes: 0,
        }
    }

    pub fn open(&self) -> usize {
        self.windows.iter().filter(|w| w.used).count()
    }

    pub fn dirty(&mut self, r: Rect) {
        self.damage = self.damage.union(&r.clamp_to_screen());
    }

    pub fn dirty_all(&mut self) {
        self.damage = Rect { x: 0, y: 0, w: WIDTH, h: HEIGHT };
    }

    /// Find a window by owner and id. The owner comes from the kernel, so this
    /// is the whole of the ownership check.
    fn find(&mut self, owner: usize, id: u8) -> Option<usize> {
        self.windows
            .iter()
            .position(|w| w.used && w.owner == owner && w.id == id)
    }

    pub fn create(&mut self, owner: usize, id: u8, rect: Rect, bg: u32, title: &[u8]) -> bool {
        if self.find(owner, id).is_some() {
            return false;
        }
        let Some(slot) = self.windows.iter().position(|w| !w.used) else {
            return false;
        };
        let w = &mut self.windows[slot];
        *w = Window::BLANK;
        w.used = true;
        w.owner = owner;
        w.id = id;
        w.rect = rect.clamp_to_screen();
        w.bg = bg;
        w.z = self.next_z;
        self.next_z += 1;
        w.title_len = title.len().min(w.title.len());
        w.title[..w.title_len].copy_from_slice(&title[..w.title_len]);
        let r = w.rect;
        self.dirty(r);
        true
    }

    /// Returns false when the sender does not own that window, which is
    /// counted rather than silently ignored.
    pub fn with<F: FnOnce(&mut Window)>(&mut self, owner: usize, id: u8, f: F) -> bool {
        match self.find(owner, id) {
            Some(i) => {
                let before = self.windows[i].rect;
                f(&mut self.windows[i]);
                let after = self.windows[i].rect;
                self.dirty(before);
                self.dirty(after);
                true
            }
            None => {
                self.rejected += 1;
                false
            }
        }
    }

    pub fn raise(&mut self, owner: usize, id: u8) -> bool {
        let z = self.next_z;
        self.next_z += 1;
        self.with(owner, id, |w| w.z = z)
    }

    pub fn push(&mut self, owner: usize, id: u8, cmd: Cmd) -> bool {
        self.with(owner, id, |w| {
            if w.ncmd < MAX_CMDS {
                w.cmds[w.ncmd] = cmd;
                w.ncmd += 1;
            }
        })
    }

    pub fn reset(&mut self, owner: usize, id: u8) -> bool {
        self.with(owner, id, |w| w.ncmd = 0)
    }

    /// Which window a point belongs to: the topmost one containing it.
    pub fn hit(&self, x: usize, y: usize) -> Option<(usize, u8, usize, usize)> {
        let mut best: Option<usize> = None;
        for (i, w) in self.windows.iter().enumerate() {
            if !w.used || !w.rect.contains(x, y) {
                continue;
            }
            if best.map_or(true, |b| w.z > self.windows[b].z) {
                best = Some(i);
            }
        }
        let i = best?;
        let w = &self.windows[i];
        Some((w.owner, w.id, x - w.rect.x, y - w.rect.y))
    }

    /// Draw every window, bottom to top, inside the clip the caller set.
    pub fn paint(&mut self, painter: &mut dyn Painter) {
        let mut order = [0usize; MAX_WINDOWS];
        let mut n = 0;
        for (i, w) in self.windows.iter().enumerate() {
            if w.used {
                order[n] = i;
                n += 1;
            }
        }
        // Insertion sort by z: eight windows, and it keeps the ordering
        // obvious at the point where being wrong would be invisible.
        for i in 1..n {
            let mut j = i;
            while j > 0 && self.windows[order[j - 1]].z > self.windows[order[j]].z {
                order.swap(j - 1, j);
                j -= 1;
            }
        }

        for &i in order[..n].iter() {
            let w = self.windows[i];
            painter.fill(w.rect.x, w.rect.y, w.rect.w, w.rect.h, w.bg);
            painter.fill(w.rect.x, w.rect.y, w.rect.w, TITLE_BAR, shade(w.bg));
            painter.text(
                w.rect.x + 6,
                w.rect.y + 7,
                1,
                0xffe6edf3,
                &w.title[..w.title_len],
            );
            for c in w.cmds[..w.ncmd].iter() {
                match *c {
                    Cmd::None => {}
                    Cmd::Fill { x, y, w: cw, h, color } => {
                        painter.fill(w.rect.x + x, w.rect.y + TITLE_BAR + y, cw, h, color)
                    }
                    Cmd::Text { x, y, scale, color, len, bytes } => painter.text(
                        w.rect.x + x,
                        w.rect.y + TITLE_BAR + y,
                        scale,
                        color,
                        &bytes[..len],
                    ),
                }
            }
        }
        self.composes += 1;
    }
}

/// Lighten a colour for the title bar, channel by channel.
fn shade(c: u32) -> u32 {
    let f = |sh: u32| {
        let v = (c >> sh) & 0xff;
        (v + (0xff - v) / 3) << sh
    };
    0xff00_0000 | f(0) | f(8) | f(16)
}

/// What the window manager needs from whoever owns the pixels. Keeping it to
/// two calls is what lets the window model be tested without a framebuffer.
pub trait Painter {
    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32);
    fn text(&mut self, x: usize, y: usize, scale: usize, color: u32, bytes: &[u8]);
}
