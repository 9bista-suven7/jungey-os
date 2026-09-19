//! PL011 UART — the kernel's console before anything else exists.
//!
//! On QEMU `virt` the primary UART is at physical 0x0900_0000, reached through
//! the higher-half linear map. Real boards differ, so `set_base` re-points the
//! console once the device tree has been read.

use crate::sync::SpinLock;
use core::fmt::{self, Write};
use core::ptr::{read_volatile, write_volatile};

/// Physical base of the PL011 QEMU `virt` gives us. Replaced from the DTB.
const UART0_PHYS_DEFAULT: usize = 0x0900_0000;

const DR: usize = 0x00; // data register
const FR: usize = 0x18; // flag register
const FR_TXFF: u32 = 1 << 5; // transmit FIFO full
const FR_RXFE: u32 = 1 << 4; // receive FIFO empty

pub struct Uart {
    pub base: usize,
}

impl Uart {
    pub const fn new(base: usize) -> Self {
        Self { base }
    }

    #[inline]
    fn reg(&self, off: usize) -> *mut u32 {
        (self.base + off) as *mut u32
    }

    pub fn put(&self, c: u8) {
        unsafe {
            while read_volatile(self.reg(FR)) & FR_TXFF != 0 {
                core::hint::spin_loop();
            }
            write_volatile(self.reg(DR), c as u32);
        }
    }

    /// Non-blocking read of one byte, if the FIFO has one.
    pub fn get(&self) -> Option<u8> {
        unsafe {
            if read_volatile(self.reg(FR)) & FR_RXFE != 0 {
                None
            } else {
                Some(read_volatile(self.reg(DR)) as u8)
            }
        }
    }
}

impl Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                self.put(b'\r');
            }
            self.put(b);
        }
        Ok(())
    }
}

/// The kernel console. Locked, because threads and IRQ handlers both print.
static CONSOLE: SpinLock<Uart> = SpinLock::new(Uart::new(crate::mm::phys_to_virt(
    UART0_PHYS_DEFAULT,
)));

/// Re-point the console at a PL011 discovered in the device tree.
pub fn set_base(phys: usize) {
    CONSOLE.lock().base = crate::mm::phys_to_virt(phys);
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let _ = CONSOLE.lock().write_fmt(args);
}

/// Print without taking the lock, for the panic path: the code that panicked
/// may well be the code holding it.
#[doc(hidden)]
pub fn _print_forced(args: fmt::Arguments) {
    // Safety: only reached from `panic`, after which nothing else runs.
    let _ = unsafe { CONSOLE.force() }.write_fmt(args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::uart::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    ()                 => ($crate::print!("\n"));
    ($($arg:tt)*)      => ($crate::print!("{}\n", format_args!($($arg)*)));
}

/// `println!` that bypasses the console lock. Panic path only.
#[macro_export]
macro_rules! println_forced {
    ($($arg:tt)*) => ($crate::uart::_print_forced(format_args!("{}\n", format_args!($($arg)*))));
}
