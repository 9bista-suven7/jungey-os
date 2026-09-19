//! Bus enumeration.
//!
//! The kernel's entire remaining involvement with hardware it does not itself
//! drive: walk the transports the device tree describes, read the identity
//! register that says what is plugged into each, and hand the matching one to a
//! driver as a capability. It knows a device's *kind*, never its protocol.
//!
//! That line — enumeration in the kernel, drivers outside it — is where every
//! microkernel puts it, and it is why the kernel below contains no virtqueue
//! code, no block-device code, and nothing that has to change when the disk
//! becomes UFS.

use crate::dtb::{DeviceNode, Fdt};
use crate::mm::phys_to_virt;
use core::ptr::read_volatile;

/// virtio-mmio identity registers (virtio 1.2 §4.2.2). The only two offsets the
/// kernel knows, and it only reads them.
const MAGIC: usize = 0x000;
const VERSION: usize = 0x004;
const DEVICE_ID: usize = 0x008;
const MAGIC_VALUE: u32 = 0x7472_6976; // "virt"

/// virtio device ids worth naming here.
pub const VIRTIO_BLOCK: u32 = 2;
pub const VIRTIO_GPU: u32 = 16;
pub const VIRTIO_INPUT: u32 = 18;

/// A transport slot with something in it.
#[derive(Clone, Copy)]
pub struct Found {
    pub node: DeviceNode,
    pub device_id: u32,
    pub version: u32,
}

/// Find the first virtio-mmio transport holding a device of kind `device_id`.
///
/// QEMU fills its slots from the last one downwards and leaves the rest
/// reporting id 0, so every slot is read.
pub fn find_virtio(fdt: &Fdt, device_id: u32) -> Option<Found> {
    let mut nodes = [DeviceNode::default(); 32];
    let n = fdt.devices("virtio,mmio", &mut nodes);

    for node in nodes[..n].iter() {
        let base = phys_to_virt(node.base as usize);
        unsafe {
            if read_volatile((base + MAGIC) as *const u32) != MAGIC_VALUE {
                continue;
            }
            let id = read_volatile((base + DEVICE_ID) as *const u32);
            if id != device_id {
                continue;
            }
            return Some(Found {
                node: *node,
                device_id: id,
                version: read_volatile((base + VERSION) as *const u32),
            });
        }
    }
    None
}

/// How many transport slots the device tree describes, for reporting.
pub fn virtio_slots(fdt: &Fdt) -> usize {
    let mut nodes = [DeviceNode::default(); 32];
    fdt.devices("virtio,mmio", &mut nodes)
}
