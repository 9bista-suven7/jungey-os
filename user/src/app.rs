//! A tappable application.
//!
//! Two processes run this with different arguments, and between them they are
//! the stage 4 exit test. What matters is not that they are interesting — they
//! are a list of rows you can toggle — but that each one is an ordinary,
//! unprivileged process holding exactly two capabilities: one to send window
//! operations to the display server, and one to receive the taps that landed
//! on its own windows.
//!
//! It cannot draw on the screen. It cannot see the framebuffer. It cannot read
//! another application's taps, or find out where its own window is, or reach a
//! window it did not create. All of that follows from the two capabilities and
//! from the display server keying windows by the sender the kernel recorded.

use crate::say;
use crate::sys::*;

const CAP_WM: usize = 0; // send: window operations
const CAP_EVENTS: usize = 1; // recv: taps on our windows

const OP_WIN_CREATE: u8 = 0x20;
const OP_WIN_RAISE: u8 = 0x21;
const OP_WIN_MOVE: u8 = 0x22;
const OP_WIN_TEXT: u8 = 0x23;
const OP_WIN_FILL: u8 = 0x24;
const OP_WIN_RESET: u8 = 0x25;
const OP_COMPOSE: u8 = 0x31;
const EV_TAP: u8 = 0x40;
/// The kernel telling this application to open its window. Without it the two
/// applications race, and which one ends up on top of the other is decided by
/// which core happened to be free — fine for a desktop, useless for a test
/// whose whole subject is stacking order.
const EV_START: u8 = 0x41;

const ROW_H: usize = 34;
const MAX_ROWS: usize = 6;
/// Taps arrive in window coordinates, and the server draws the title bar, so
/// its height is part of the protocol. A cleaner design would send the content
/// origin with the event; this is the honest version of a shortcut.
const TITLE_BAR: usize = 22;
const INSET: usize = 10;

/// A message under construction, in the application's own memory.
struct Msg {
    buf: [u8; 512],
    len: usize,
}

impl Msg {
    fn new() -> Msg {
        Msg { buf: [0; 512], len: 0 }
    }
    fn b(&mut self, v: u8) -> &mut Msg {
        if self.len < self.buf.len() {
            self.buf[self.len] = v;
            self.len += 1;
        }
        self
    }
    fn be16(&mut self, v: usize) -> &mut Msg {
        self.b((v >> 8) as u8).b(v as u8)
    }
    fn le32(&mut self, v: u32) -> &mut Msg {
        for byte in v.to_le_bytes() {
            self.b(byte);
        }
        self
    }
    fn text(&mut self, s: &str) -> &mut Msg {
        let n = s.len().min(40);
        self.b(n as u8);
        for &c in s.as_bytes()[..n].iter() {
            self.b(c);
        }
        self
    }
    fn flush(&mut self) -> bool {
        let ok = send(CAP_WM, &self.buf[..self.len]).is_ok();
        self.len = 0;
        ok
    }
}

pub struct App {
    pub tag: &'static str,
    pub win: u8,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub bg: u32,
    pub accent: u32,
    pub title: &'static str,
    pub rows: &'static [&'static str],
    /// A window id this process does not own, which it tries once. The point
    /// is to be refused.
    pub poach: Option<u8>,
}

fn draw(app: &App, on: &[bool; MAX_ROWS]) {
    let mut m = Msg::new();
    m.b(OP_WIN_RESET).b(app.win);
    for (i, row) in app.rows.iter().enumerate().take(MAX_ROWS) {
        let y = 10 + i * ROW_H;
        let color = if on[i] { app.accent } else { 0xff30363d };
        m.b(OP_WIN_FILL)
            .b(app.win)
            .be16(10)
            .be16(y)
            .be16(app.w.saturating_sub(20))
            .be16(ROW_H - 8)
            .le32(color);
        m.b(OP_WIN_TEXT)
            .b(app.win)
            .be16(20)
            .be16(y + 9)
            .le32(0xffe6edf3)
            .b(1)
            .text(row);
        // One message per two rows keeps every message well under the
        // server's buffer without needing to know its size exactly.
        if m.len > 300 {
            m.flush();
        }
    }
    m.b(OP_COMPOSE);
    m.flush();
}

pub fn run(app: &App) {
    // Wait to be told to open. Anything else that arrives before that is
    // dropped: there is no window yet for a tap to belong to.
    let mut ev = [0u8; 32];
    loop {
        match recv(CAP_EVENTS, &mut ev) {
            Ok(n) if n >= 1 && ev[0] == EV_START => break,
            Ok(_) => continue,
            Err(_) => return,
        }
    }

    let mut m = Msg::new();
    m.b(OP_WIN_CREATE)
        .b(app.win)
        .be16(app.x)
        .be16(app.y)
        .be16(app.w)
        .be16(app.h)
        .le32(app.bg)
        .text(app.title);
    if !m.flush() {
        say(app.tag, " could not reach the display server");
        return;
    }

    let mut on = [false; MAX_ROWS];
    draw(app, &on);

    let mut l = Line::new();
    l.s(app.tag).s(" pid ").d(getpid()).s(" opened window ").d(app.win as usize).s(", ")
        .d(app.rows.len()).s(" rows, holding 2 capabilities").nl();

    // Try, once, to move a window belonging to somebody else. The display
    // server looks the window up by (sender, id), so this finds nothing —
    // and the other application's window does not move.
    if let Some(id) = app.poach {
        let mut m = Msg::new();
        m.b(OP_WIN_MOVE).b(id).be16(0).be16(0).b(OP_COMPOSE);
        m.flush();
        let mut l = Line::new();
        l.s(app.tag).s(" asked the display server to move window ").d(id as usize)
            .s(", which belongs to another process").nl();
    }

    let mut taps = 0;
    loop {
        // A failed receive means the capability was revoked: the session is
        // over, and this is how an application finds out.
        let Ok(n) = recv(CAP_EVENTS, &mut ev) else {
            let mut l = Line::new();
            l.s(app.tag).s(" display session ended after ").d(taps).s(" taps").nl();
            return;
        };
        if n < 7 || ev[0] != EV_TAP {
            continue;
        }
        let ry = ((ev[4] as usize) << 8) | ev[5] as usize;
        taps += 1;

        // Tapping a window brings it to the front, which is what makes the
        // next tap at the same point go somewhere else.
        let mut m = Msg::new();
        m.b(OP_WIN_RAISE).b(app.win);
        m.flush();

        if ry < TITLE_BAR + INSET {
            let mut l = Line::new();
            l.s(app.tag).s(" title bar tapped; raised").nl();
            continue;
        }
        let row = (ry - TITLE_BAR - INSET) / ROW_H;
        if row < app.rows.len() && row < MAX_ROWS {
            on[row] = !on[row];
            draw(app, &on);
            let mut l = Line::new();
            l.s(app.tag).s(" row ").d(row).s(" \"").s(app.rows[row]).s("\" is now ")
                .s(if on[row] { "on" } else { "off" }).nl();
        } else {
            let mut l = Line::new();
            l.s(app.tag).s(" tap below the last row, ignored").nl();
        }
    }
}

pub const SHELL: App = App {
    tag: "  [shell   ]",
    win: 0,
    x: 30,
    y: 150,
    w: 390,
    h: 250,
    bg: 0xff161b22,
    accent: 0xff58a6ff,
    title: "JUNGEY SHELL",
    rows: &[
        "RUN A MODEL",
        "OPEN NOTES",
        "SHOW THE LOG",
        "CHECK BATTERY",
        "READ THE AUDIT",
        "SLEEP",
    ],
    poach: None,
};

pub const NOTES: App = App {
    tag: "  [notes   ]",
    win: 1,
    x: 70,
    y: 330,
    w: 390,
    h: 220,
    bg: 0xff1b1622,
    accent: 0xffd29922,
    title: "NOTES",
    rows: &["GROCERIES", "STANDUP", "READ THE ROADMAP"],
    // The shell's window. Asking to move it is the point.
    poach: Some(0),
};
