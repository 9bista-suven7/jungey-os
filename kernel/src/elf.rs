//! Minimal ELF64 loader for AArch64 user images.
//!
//! Only what a statically linked, non-relocatable executable needs: walk the
//! program headers, map each PT_LOAD with the permissions its flags ask for,
//! copy the file bytes in, zero the rest. No dynamic linking, no relocations,
//! no interpreter — those belong to a userspace loader, not the kernel.

use crate::mm::paging::{AddressSpace, Perm};
use crate::mm::{page_align_down, page_align_up, phys_to_virt, PAGE_SIZE};

const EI_NIDENT: usize = 16;
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;

const PF_X: u32 = 1;
const PF_W: u32 = 2;

#[repr(C)]
struct Ehdr {
    ident: [u8; EI_NIDENT],
    etype: u16,
    machine: u16,
    version: u32,
    entry: u64,
    phoff: u64,
    shoff: u64,
    flags: u32,
    ehsize: u16,
    phentsize: u16,
    phnum: u16,
    shentsize: u16,
    shnum: u16,
    shstrndx: u16,
}

#[repr(C)]
struct Phdr {
    ptype: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    paddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

fn read<T>(image: &[u8], at: usize) -> Result<&T, &'static str> {
    if at + core::mem::size_of::<T>() > image.len() {
        return Err("ELF truncated");
    }
    Ok(unsafe { &*(image.as_ptr().add(at) as *const T) })
}

/// Map `image` into `space`. Returns the entry point.
pub fn load(space: &mut AddressSpace, image: &[u8]) -> Result<usize, &'static str> {
    let eh: &Ehdr = read(image, 0)?;

    if eh.ident[..4] != ELF_MAGIC {
        return Err("not an ELF");
    }
    if eh.ident[4] != ELFCLASS64 || eh.ident[5] != ELFDATA2LSB {
        return Err("not 64-bit little-endian");
    }
    if eh.etype != ET_EXEC {
        return Err("not a static executable");
    }
    if eh.machine != EM_AARCH64 {
        return Err("wrong architecture");
    }

    for i in 0..eh.phnum as usize {
        let ph: &Phdr = read(image, eh.phoff as usize + i * eh.phentsize as usize)?;
        if ph.ptype != PT_LOAD || ph.memsz == 0 {
            continue;
        }
        if ph.filesz > ph.memsz {
            return Err("segment filesz exceeds memsz");
        }

        let vaddr = ph.vaddr as usize;
        let start = page_align_down(vaddr);
        let end = page_align_up(vaddr + ph.memsz as usize);
        let pages = (end - start) / PAGE_SIZE;

        // Permissions come from the segment, not from a guess: writable
        // segments are never executable and executable ones are never writable.
        let perm = if ph.flags & PF_X != 0 {
            Perm::UserText
        } else if ph.flags & PF_W != 0 {
            Perm::UserData
        } else {
            Perm::UserReadOnly
        };

        space.map_anonymous(start, pages, perm)?;

        // Write the file bytes in through the linear map. The kernel's own
        // view of these frames is writable even where the user's is not, which
        // is exactly what makes read-only text loadable.
        let src_end = (ph.offset + ph.filesz) as usize;
        if src_end > image.len() {
            return Err("segment past end of image");
        }
        let bytes = &image[ph.offset as usize..src_end];
        let mut written = 0;
        while written < bytes.len() {
            let va = vaddr + written;
            let pa = space.translate(va).ok_or("segment page vanished")?;
            let n = (PAGE_SIZE - (va & (PAGE_SIZE - 1))).min(bytes.len() - written);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes[written..].as_ptr(),
                    phys_to_virt(pa) as *mut u8,
                    n,
                )
            };
            written += n;
        }
        // Anything past filesz is already zero: map_anonymous zeroes frames.
    }

    Ok(eh.entry as usize)
}
