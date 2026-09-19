//! PL011 UART — the kernel's console before anything else exists.
//!
//! On QEMU `virt` the primary UART is at 0x0900_0000. Real boards differ; once
//! the device tree parser lands (`dtb.rs`) this base is discovered, not assumed.

use core::fmt::{self, Write};
use core::ptr::{read_volatile, write_volatile};

const UART0_BASE: usize = 0x0900_0000;

const DR: usize = 0x00; // data register
const FR: usize = 0x18; // flag register
const FR_TXFF: u32 = 1 << 5; // transmit FIFO full
const FR_RXFE: u32 = 1 << 4; // receive FIFO empty

pub struct Uart {
    base: usize,
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

/// Single-core console. Becomes a proper lock once secondaries are up.
pub static mut CONSOLE: Uart = Uart::new(UART0_BASE);

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    // Safety: single-threaded until SMP bring-up.
    unsafe {
        let c = &raw mut CONSOLE;
        let _ = (*c).write_fmt(args);
    }
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
