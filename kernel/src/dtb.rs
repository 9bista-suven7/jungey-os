//! Minimal flattened-device-tree (FDT / DTB) reader.
//!
//! The kernel refuses to hardcode board layout: RAM ranges, the UART base, the
//! interrupt controller and later the NPU/GPU nodes all come from here. This is
//! a read-only, allocation-free parser over the blob the bootloader handed us.
//!
//! Spec: <https://devicetree-specification.readthedocs.io>

const FDT_MAGIC: u32 = 0xd00d_feed;

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

#[inline]
unsafe fn be32(p: *const u8) -> u32 {
    u32::from_be_bytes([*p, *p.add(1), *p.add(2), *p.add(3)])
}

#[inline]
const fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Read a NUL-terminated string starting at `p`, returning it and its length
/// including the terminator.
unsafe fn cstr(p: *const u8) -> (&'static str, usize) {
    let mut len = 0;
    while *p.add(len) != 0 {
        len += 1;
    }
    let bytes = core::slice::from_raw_parts(p, len);
    (core::str::from_utf8(bytes).unwrap_or("<non-utf8>"), len + 1)
}

pub struct Fdt {
    base: *const u8,
    struct_off: usize,
    struct_size: usize,
    strings_off: usize,
    total_size: usize,
}

impl Fdt {
    /// Validate the blob at `addr`. Returns `None` if the magic doesn't match.
    pub fn new(addr: usize) -> Option<Self> {
        if addr == 0 || addr & 7 != 0 {
            return None;
        }
        let base = addr as *const u8;
        unsafe {
            if be32(base) != FDT_MAGIC {
                return None;
            }
            Some(Fdt {
                base,
                total_size: be32(base.add(4)) as usize,
                struct_off: be32(base.add(8)) as usize,
                strings_off: be32(base.add(12)) as usize,
                struct_size: be32(base.add(36)) as usize,
            })
        }
    }

    pub fn total_size(&self) -> usize {
        self.total_size
    }

    unsafe fn str_at(&self, off: usize) -> &'static str {
        cstr(self.base.add(self.strings_off + off)).0
    }

    /// Value bytes of a property on the root node, e.g. "model".
    pub fn root_prop(&self, want: &str) -> Option<&'static [u8]> {
        let mut w = Walker::new(self);
        while let Some(ev) = w.next_event() {
            match ev {
                Event::Prop { depth, name, value } if depth == 1 && name == want => {
                    return Some(value)
                }
                // Past the root's own properties once we descend.
                Event::BeginNode { depth } if depth == 2 => return None,
                _ => {}
            }
        }
        None
    }

    /// The board's human-readable model string, if it advertises one.
    pub fn model(&self) -> Option<&'static str> {
        let v = self.root_prop("model")?;
        let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
        core::str::from_utf8(&v[..end]).ok()
    }

    /// Every `(base, size)` pair from every `/memory` node.
    pub fn memory_regions(&self) -> MemoryRegions<'_> {
        MemoryRegions {
            walker: Walker::new(self),
            cells: None,
            addr_cells: 2,
            size_cells: 2,
        }
    }
}

enum Event {
    BeginNode {
        depth: usize,
    },
    EndNode,
    Prop {
        depth: usize,
        name: &'static str,
        value: &'static [u8],
    },
}

/// Linear token walker over the FDT structure block.
struct Walker<'a> {
    fdt: &'a Fdt,
    pos: usize,
    depth: usize,
    node: &'static str,
}

impl<'a> Walker<'a> {
    fn new(fdt: &'a Fdt) -> Self {
        Walker { fdt, pos: 0, depth: 0, node: "" }
    }

    fn next_event(&mut self) -> Option<Event> {
        loop {
            if self.pos + 4 > self.fdt.struct_size {
                return None;
            }
            let p = unsafe { self.fdt.base.add(self.fdt.struct_off + self.pos) };
            let token = unsafe { be32(p) };
            self.pos += 4;

            match token {
                FDT_NOP => continue,
                FDT_END => return None,
                FDT_BEGIN_NODE => {
                    let (name, n) =
                        unsafe { cstr(self.fdt.base.add(self.fdt.struct_off + self.pos)) };
                    self.pos += align4(n);
                    self.depth += 1;
                    self.node = name;
                    return Some(Event::BeginNode { depth: self.depth });
                }
                FDT_END_NODE => {
                    self.depth = self.depth.saturating_sub(1);
                    return Some(Event::EndNode);
                }
                FDT_PROP => {
                    let hdr = unsafe { self.fdt.base.add(self.fdt.struct_off + self.pos) };
                    let len = unsafe { be32(hdr) } as usize;
                    let nameoff = unsafe { be32(hdr.add(4)) } as usize;
                    self.pos += 8;
                    let value = unsafe {
                        core::slice::from_raw_parts(
                            self.fdt.base.add(self.fdt.struct_off + self.pos),
                            len,
                        )
                    };
                    self.pos += align4(len);
                    let name = unsafe { self.fdt.str_at(nameoff) };
                    return Some(Event::Prop { depth: self.depth, name, value });
                }
                _ => return None, // corrupt blob
            }
        }
    }

    fn in_memory_node(&self) -> bool {
        self.depth == 2 && (self.node == "memory" || self.node.starts_with("memory@"))
    }
}

/// Iterator over `(base, size)` pairs found in `/memory` nodes.
pub struct MemoryRegions<'a> {
    walker: Walker<'a>,
    cells: Option<(&'static [u8], usize)>, // (reg blob, byte cursor)
    addr_cells: usize,
    size_cells: usize,
}

impl<'a> MemoryRegions<'a> {
    fn read_cells(blob: &[u8], at: usize, cells: usize) -> u64 {
        let mut v = 0u64;
        for i in 0..cells {
            let o = at + i * 4;
            v = (v << 32) | u32::from_be_bytes([blob[o], blob[o + 1], blob[o + 2], blob[o + 3]]) as u64;
        }
        v
    }
}

impl<'a> Iterator for MemoryRegions<'a> {
    type Item = (u64, u64);

    fn next(&mut self) -> Option<(u64, u64)> {
        loop {
            // Drain the reg property we're part-way through.
            if let Some((blob, cursor)) = self.cells {
                let stride = (self.addr_cells + self.size_cells) * 4;
                if cursor + stride <= blob.len() {
                    let base = Self::read_cells(blob, cursor, self.addr_cells);
                    let size =
                        Self::read_cells(blob, cursor + self.addr_cells * 4, self.size_cells);
                    self.cells = Some((blob, cursor + stride));
                    if size > 0 {
                        return Some((base, size));
                    }
                    continue;
                }
                self.cells = None;
            }

            match self.walker.next_event()? {
                Event::Prop { depth: 1, name: "#address-cells", value } if value.len() >= 4 => {
                    self.addr_cells =
                        u32::from_be_bytes([value[0], value[1], value[2], value[3]]) as usize;
                }
                Event::Prop { depth: 1, name: "#size-cells", value } if value.len() >= 4 => {
                    self.size_cells =
                        u32::from_be_bytes([value[0], value[1], value[2], value[3]]) as usize;
                }
                Event::Prop { name: "reg", value, .. } if self.walker.in_memory_node() => {
                    self.cells = Some((value, 0));
                }
                _ => {}
            }
        }
    }
}
