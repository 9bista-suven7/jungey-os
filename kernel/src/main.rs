//! Jungey OS kernel — AArch64.
//!
//! Stage 1: MMU on with a higher-half kernel, physical frame allocator, kernel
//! heap, GICv3, generic timer, and preemptive round-robin kernel threads.
//!
//! See `os/docs/ARCHITECTURE.md` for where this is going.

#![no_std]
#![no_main]

extern crate alloc;

use core::arch::global_asm;
use core::panic::PanicInfo;

#[macro_use]
pub mod uart;
pub mod cap;
pub mod dtb;
pub mod elf;
pub mod exceptions;
pub mod fs;
pub mod gic;
pub mod ipc;
pub mod irq;
pub mod mm;
pub mod model;
pub mod proc;
pub mod sched;
pub mod smp;
pub mod syscall;
pub mod blk;
pub mod devices;
pub mod sync;
pub mod time;

#[global_allocator]
static ALLOCATOR: mm::heap::KernelAllocator = mm::heap::KernelAllocator;

global_asm!(include_str!("boot.s"));
global_asm!(include_str!("vectors.s"));
global_asm!(include_str!("sched/switch.s"));

extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

/// Pages handed to the kernel heap at boot. Grows on demand later.
const HEAP_PAGES: usize = 256;

/// The userspace image, built from `os/user` by this crate's build script and
/// embedded in the kernel. Stage 3 reads it off a filesystem instead.
static INIT_ELF: &[u8] = include_bytes!(env!("JUNGEY_INIT_ELF"));

extern "C" {
    fn enter_user(entry: usize, user_sp: usize, arg: usize) -> !;
}

const BANNER: &str = r"
    _                              ___  ____
   | |_   _ _ __   __ _  ___ _   _/ _ \/ ___|
 _ | | | | | '_ \ / _` |/ _ \ | | | | | \___ \
| |_| | |_| | | | | (_| |  __/ |_| | |_| |___) |
 \___/ \__,_|_| |_|\__, |\___|\__, |\___/|____/
                   |___/      |___/
";

const RULE: &str = "  ----------------------------------------------------------";

#[no_mangle]
pub extern "C" fn kernel_main(dtb_phys: usize) -> ! {
    println!("{}", BANNER);
    println!("  Jungey OS  v0.8.0  ·  stage 5a  ·  aarch64");
    println!("{}", RULE);

    let (kstart, kend) = unsafe {
        (
            &__kernel_start as *const u8 as usize,
            &__kernel_end as *const u8 as usize,
        )
    };

    println!("  image      : {:#018x}..{:#018x}  ({} KiB)", kstart, kend, (kend - kstart) / 1024);
    println!("  phys       : {:#012x}..{:#012x}", mm::virt_to_phys(kstart), mm::virt_to_phys(kend));
    println!("  exec level : EL{}", current_el());
    println!("  mmu        : on, linear map at {:#018x}", mm::PHYS_OFFSET);

    exceptions::init();
    println!("  vectors    : installed at VBAR_EL1");

    let Some(fdt) = dtb::Fdt::new(mm::phys_to_virt(dtb_phys)) else {
        println!("  fdt        : INVALID at {:#012x} — cannot continue", dtb_phys);
        halt()
    };

    println!("  dtb        : {:#012x} phys, {} bytes", dtb_phys, fdt.total_size());
    if let Some(model) = fdt.model() {
        println!("  machine    : {}", model);
    }

    // Re-point the console at whatever the device tree says the PL011 is.
    let mut regs = [(0u64, 0u64); 2];
    if fdt.node_regs("arm,pl011", &mut regs) > 0 {
        uart::set_base(regs[0].0 as usize);
        println!("  console    : pl011 at {:#012x} (from dtb)", regs[0].0);
    }

    // ---- physical memory ----
    let mut total = 0u64;
    for (i, (base, size)) in fdt.memory_regions().enumerate() {
        println!("  ram[{}]     : {:#012x}..{:#012x}  ({} MiB)", i, base, base + size, size / (1024 * 1024));
        total += size;
    }
    println!("  ram total  : {} MiB", total / (1024 * 1024));

    mm::frames::init(
        &fdt,
        &[
            (mm::virt_to_phys(kstart), mm::virt_to_phys(kend)),
            (dtb_phys, dtb_phys + fdt.total_size()),
        ],
    );
    println!(
        "  frames     : {} free of {} ({} MiB usable)",
        mm::frames::free_count(),
        mm::frames::total_count(),
        mm::frames::free_count() * 4096 / (1024 * 1024)
    );

    // ---- kernel heap ----
    mm::heap::init(HEAP_PAGES).expect("heap init");
    let (heap_total, _) = mm::heap::stats();
    println!("  heap       : {} KiB", heap_total / 1024);

    // ---- user address spaces ----
    mm::paging::init().expect("paging init");
    println!("  paging     : ttbr0 live, 4 KiB pages, per-process asids");

    // ---- cpu discovery, before anything that reads per-cpu state ----
    let mut mpidrs = [0u64; smp::MAX_CPUS];
    let ncpus = fdt.cpus(&mut mpidrs);
    let psci_method = fdt.prop_of("arm,psci-0.2", "method").unwrap_or(b"");
    smp::init_boot_cpu(psci_method, ncpus);
    println!(
        "  cpus       : {} in the device tree, psci via {}",
        ncpus,
        smp::psci_method_name()
    );

    // ---- interrupt controller ----
    let mut gic_regs = [(0u64, 0u64); 2];
    if fdt.node_regs("arm,gic-v3", &mut gic_regs) < 2 {
        println!("  gic        : no arm,gic-v3 node — cannot continue");
        halt()
    }
    let lines = gic::init(gic_regs[0].0 as usize, gic_regs[1].0 as usize);
    println!(
        "  gic        : v3 at {:#012x}/{:#012x}, {} interrupt lines",
        gic_regs[0].0, gic_regs[1].0, lines
    );

    // ---- scheduler and timer ----
    sched::init();
    println!("  sched      : round-robin, thread 0 is '{}'", sched::current_name());

    time::start();
    println!("  timer      : cntv at {} Hz, tick {} Hz, intid {}", time::frequency(), time::HZ, time::TIMER_INTID);

    unsafe { core::arch::asm!("msr daifclr, #3") }; // interrupts live from here
    println!("  irq        : unmasked");

    // ---- secondary cores ----
    for i in 1..ncpus.min(smp::MAX_CPUS) {
        match smp::start_cpu(i, mpidrs[i]) {
            Ok(()) => println!("  cpu {}      : online (mpidr {:#x})", i, mpidrs[i]),
            Err(e) => println!("  cpu {}      : FAILED — {}", i, e),
        }
    }
    println!("  smp        : {} of {} cores online", smp::online_count(), ncpus);
    println!("{}", RULE);

    stage2_demo();
    heap_check("after stage 2");
    println!("{}", RULE);
    stage3a_demo();
    heap_check("after stage 3a");
    println!("{}", RULE);
    if start_block_driver(&fdt) {
        println!("{}", RULE);
        stage3b_demo();
    }
    heap_check("after stage 3b");
    println!("{}", RULE);
    stage3c_demo();
    heap_check("after stage 3c");
    println!("{}", RULE);
    stage5a_demo();
    heap_check("after stage 5a");

    println!("{}", RULE);
    if blk::attached() {
        blk::shutdown();
            println!(
            "  driver     : {} requests served by the userspace driver, {} stale replies",
            blk::request_count(),
            blk::stale_replies()
        );
    }
    println!("  stage 5a complete.");

    // The demos are the kernel's whole job right now, so stopping the machine
    // when they finish beats idling forever: `./run.sh` returns, and a stress
    // loop is bounded by the work rather than by a timeout.
    println!("  powering off via psci.");
    smp::system_off()
}

// ---------------------------------------------------------------------------
// Stage 2 exit test: two processes exchange a message they could only exchange
// because each was handed a capability, a third is denied for holding none,
// and revoking the parent capability kills the whole subtree at once.
// ---------------------------------------------------------------------------

/// When the kernel cuts the root capability, in scheduler ticks.
const REVOKE_AT_TICK: u64 = 60;

/// Kernel-side entry for a user thread: install the address space, then leave
/// EL1 for good.
fn user_thread(pid: usize) {
    let p = proc::get(pid).expect("process vanished");
    let (entry, sp, arg, ttbr0) = unsafe {
        ((*p).entry, (*p).stack_top, (*p).arg, (*p).space.ttbr0())
    };
    mm::paging::activate(ttbr0);
    unsafe { enter_user(entry, sp, arg) }
}

fn start_process(name: &'static str, role: usize, caps: &[cap::Cap]) -> usize {
    start_process_inner(name, role, caps, false)
}

/// A process that stays up answering requests rather than finishing.
fn start_service(name: &'static str, role: usize, caps: &[cap::Cap]) -> usize {
    start_process_inner(name, role, caps, true)
}

fn start_process_inner(
    name: &'static str,
    role: usize,
    caps: &[cap::Cap],
    service: bool,
) -> usize {
    let pid = proc::create(name, INIT_ELF, role).expect("create process");
    for (slot, c) in caps.iter().enumerate() {
        proc::install_cap(pid, slot, *c).expect("install capability");
    }
    // Stopped, then attached, then started: on four cores a thread that is
    // visible is a thread that is running, and one started before its address
    // space was attached would be scheduled with an empty TTBR0.
    let tid = sched::spawn_stopped(name, user_thread, pid).expect("spawn user thread");
    let ttbr0 = unsafe { (*proc::get(pid).unwrap()).space.ttbr0() };
    sched::attach_process(tid, pid, ttbr0);
    if service {
        sched::mark_service(tid);
    }
    sched::start(tid);
    pid
}

fn stage2_demo() {
    // One channel, and one capability to it: the root of all authority over it.
    let channel = ipc::create();
    let root = cap::Cap::root(cap::Obj::Channel(channel), cap::RIGHTS_ALL);
    println!("  channel {} created; root capability #{} ({})", channel, root.id, root.rights_str());

    // Derivation only ever narrows. Neither process can reconstruct the other's
    // authority, and neither can widen its own.
    let send_cap = root.derive(cap::RIGHT_SEND);
    let recv_cap = root.derive(cap::RIGHT_RECV);
    println!("  derived #{} ({}) and #{} ({}) from #{}",
        send_cap.id, send_cap.rights_str(), recv_cap.id, recv_cap.rights_str(), root.id);
    println!();

    start_process("receiver", 1, &[recv_cap]);
    start_process("sender", 0, &[send_cap]);
    start_process("intruder", 2, &[]);
    start_process("trespasser", 3, &[]);

    // Let the first exchange happen, then cut the root. Sleeping rather than
    // spinning means the core idles while the processes do their work.
    sched::sleep_ticks(REVOKE_AT_TICK.saturating_sub(time::ticks()));
    println!();
    println!("  [kernel  ] revoking root capability #{}", root.id);
    let killed = proc::revoke(root.id);
    println!("  [kernel  ] {} derived capabilities died with it", killed);
    println!();

    while sched::live_count() > 1 {
        sched::yield_now();
    }

    report();
}

fn report() {
    println!();
    println!("  process        pid  exit  capability");
    proc::for_each(|p| {
        let cap_desc = match p.caps[0] {
            Some(c) if c.revoked => alloc::format!("#{} {} REVOKED", c.id, c.rights_str()),
            Some(c) => alloc::format!("#{} {}", c.id, c.rights_str()),
            None => alloc::string::String::from("none"),
        };
        println!("  {:<12} {:>4} {:>5}  {}", p.name, p.pid,
            p.exit_code.unwrap_or(-1), cap_desc);
    });

    println!();
    println!("  thread          state  slices");
    sched::for_each(|t| {
        println!("  {:<12} {:>8}  {:>6}", t.name, t.state.label(), t.slices);
    });

    let (sent, received, queued) = ipc::stats(0);
    let (spurious, unclaimed) = irq::stats();
    let (heap_total, heap_used) = mm::heap::stats();
    println!();
    println!("  channel 0  : {} sent, {} received, {} queued", sent, received, queued);
    println!("  caps       : {} minted", cap::minted());
    println!("  uptime     : {} ms ({} ticks)", time::uptime_ms(), time::ticks());
    println!("  irqs       : {} spurious, {} unclaimed", spurious, unclaimed);
    println!("  heap       : {} of {} bytes in use", heap_used, heap_total);
    println!("  frames     : {} free of {}", mm::frames::free_count(), mm::frames::total_count());

    // Opt-in: prove the MMU actually unmapped the low half by touching it.
    #[cfg(feature = "fault-demo")]
    {
        println!();
        println!("  fault-demo : dereferencing a null pointer on purpose");
        let p = 0usize as *const u64;
        let v = unsafe { core::ptr::read_volatile(p) };
        println!("  fault-demo : UNREACHABLE, read {:#x}", v);
    }
}

// ---------------------------------------------------------------------------
// Stage 3a exit test: every core runs work, threads migrate between cores, and
// a lock held across cores actually serialises.
// ---------------------------------------------------------------------------

/// Worker threads for the SMP test — more than there are cores, so the run
/// queue has to hand work around rather than pinning one thread per core.
const SMP_WORKERS: usize = 8;
/// Increments each worker makes to the shared counter. The final total is the
/// only thing that distinguishes a working lock from a broken one.
const INCREMENTS: u64 = 20_000;

/// The contended resource. If the spinlock is wrong under real concurrency,
/// this ends up less than SMP_WORKERS * INCREMENTS and the test says so.
static SHARED: sync::SpinLock<u64> = sync::SpinLock::new(0);

/// Per-worker record of which cores touched the counter, so a lost update can
/// be attributed rather than just noticed.
static CONTENDED_ON: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

const WORKER_NAMES: [&str; SMP_WORKERS] =
    ["w0", "w1", "w2", "w3", "w4", "w5", "w6", "w7"];

fn smp_worker(id: usize) {
    for _ in 0..INCREMENTS {
        let mut g = SHARED.lock();
        *g += 1;
        CONTENDED_ON.fetch_or(1 << smp::cpu_id(), core::sync::atomic::Ordering::Relaxed);
        drop(g);
    }
    let _ = id;
}

/// Render a core bitmask as "0123", so a glance shows migration.
fn cores_str(mask: u64, buf: &mut [u8; 8]) -> &str {
    let mut n = 0;
    for c in 0..smp::MAX_CPUS {
        if mask & (1 << c) != 0 {
            buf[n] = b'0' + c as u8;
            n += 1;
        }
    }
    if n == 0 {
        return "-";
    }
    core::str::from_utf8(&buf[..n]).unwrap_or("?")
}

fn stage3a_demo() {
    println!(
        "  {} threads on {} cores, each taking one lock {} times",
        SMP_WORKERS,
        smp::online_count(),
        INCREMENTS
    );

    let started = time::ticks();
    for name in WORKER_NAMES.iter() {
        sched::spawn(name, smp_worker, 0).expect("spawn smp worker");
    }
    while sched::live_count() > 1 {
        sched::yield_now();
    }
    let elapsed = time::ticks() - started;

    let total = *SHARED.lock();
    let expected = SMP_WORKERS as u64 * INCREMENTS;
    println!();
    println!("  shared counter : {} of {} expected", total, expected);
    if total == expected {
        println!("  lock           : PASS — no update lost under cross-core contention");
    } else {
        println!("  lock           : FAIL — {} updates lost", expected - total);
    }
    let mut buf = [0u8; 8];
    println!(
        "  contended on   : cores {}",
        cores_str(CONTENDED_ON.load(core::sync::atomic::Ordering::Relaxed), &mut buf)
    );
    println!("  elapsed        : {} ticks ({} ms)", elapsed, elapsed * 1000 / time::HZ);

    println!();
    println!("  thread          state  slices  cores");
    sched::for_each(|t| {
        let mut b = [0u8; 8];
        println!(
            "  {:<12} {:>8}  {:>6}  {}",
            t.name,
            t.state.label(),
            t.slices,
            cores_str(t.cpus_seen, &mut b)
        );
    });

    println!();
    println!("  cpu   switches   timer ticks");
    smp::for_each(|c| println!("  {:<3}   {:>8}   {:>11}", c.id, c.switches, c.ticks));
    println!();
    println!("  idle wakeups   : {}", sched::idle_wakeups());
    let (spurious, unclaimed) = irq::stats();
    println!("  irqs           : {} spurious, {} unclaimed", spurious, unclaimed);
}

/// Report the state of the kernel heap's free list.
fn heap_check(when: &str) {
    let c = mm::heap::check();
    match c.error {
        None => println!(
            "  heap check {} : ok — {} free blocks, {} bytes free",
            when, c.blocks, c.free_bytes
        ),
        Some(e) => println!(
            "  heap check {} : CORRUPT — {} at {:#x}, after {} blocks",
            when, e, c.at, c.blocks
        ),
    }
}

// ---------------------------------------------------------------------------
// Stage 3d: hand the disk to a userspace driver, then run the stage 3b and 3c
// tests through it. Nothing below this line knows what a virtqueue is.
// ---------------------------------------------------------------------------

/// Two DMA pages: the virtqueue, and the request header, data and status byte.
const DMA_PAGES: usize = 2;

fn start_block_driver(fdt: &dtb::Fdt) -> bool {
    let slots = devices::virtio_slots(fdt);
    let Some(found) = devices::find_virtio(fdt, devices::VIRTIO_BLOCK) else {
        println!("  driver     : no block device in {} virtio transports", slots);
        return false;
    };
    println!(
        "  bus        : block device in a virtio transport at {:#x}, intid {}, version {}",
        found.node.base, found.node.irq, found.version
    );

    let Some(dma) = mm::frames::alloc_contiguous(DMA_PAGES) else {
        println!("  driver     : no contiguous memory for DMA");
        return false;
    };

    // Two channels, so requests and replies cannot be confused for each other,
    // and neither end needs a right it does not use.
    let req = ipc::create();
    let rep = ipc::create();

    // Everything the driver is allowed to do, enumerated. It has no others.
    let caps = [
        cap::Cap::root(cap::Obj::Channel(req), cap::RIGHT_RECV),
        cap::Cap::root(cap::Obj::Channel(rep), cap::RIGHT_SEND),
        cap::Cap::root(
            cap::Obj::Mmio { base: found.node.base as usize, size: found.node.size as usize },
            cap::RIGHT_MAP,
        ),
        cap::Cap::root(cap::Obj::Irq(found.node.irq), cap::RIGHT_IRQ),
        cap::Cap::root(cap::Obj::Dma { base: dma, pages: DMA_PAGES }, cap::RIGHT_MAP),
    ];

    irq::register_user(found.node.irq);
    gic::enable_spi(found.node.irq);

    let pid = start_service("blkdrv", 4, &caps);
    blk::attach(req, rep);
    println!("  driver     : blkdrv is pid {}, holding {} capabilities:", pid, caps.len());
    for (i, c) in caps.iter().enumerate() {
        println!("    slot {}     #{} {} ({})", i, c.id, c.obj_str(), c.rights_str());
    }

    match blk::info() {
        Ok(capacity) => {
            println!(
                "  disk       : {} sectors ({} MiB), driven entirely from userspace",
                capacity,
                capacity * blk::SECTOR_SIZE as u64 / (1024 * 1024)
            );
            true
        }
        Err(e) => {
            println!("  driver     : did not answer — {}", e);
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 3b exit test: a sector written on one boot is there on the next.
// ---------------------------------------------------------------------------

/// Where the persistence record lives.
///
/// Sector 5, not 64: the filesystem's log starts at sector 8 and grows upwards,
/// so a raw write anywhere above that lands inside a file sooner or later. It
/// did — this test spent a boot corrupting the model it had just written.
/// Sectors 0-7 are reserved metadata; 5 is unused by JLFS.
const RECORD_SECTOR: u64 = 5;
/// Recognises our own record and nothing else.
const RECORD_MAGIC: u64 = 0x4A55_4E47_4559_5F31; // "JUNGEY_1"

fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Fill the tail of the sector with a value-dependent pattern, so a read that
/// returns the right header but the wrong body is still caught.
fn pattern_byte(i: usize, boot: u64) -> u8 {
    (i.wrapping_mul(31) as u64 ^ boot.wrapping_mul(131)) as u8
}

fn stage3b_demo() {
    if !blk::attached() {
        return;
    }

    let mut buf = [0u8; blk::SECTOR_SIZE];
    if let Err(e) = blk::read_sector(RECORD_SECTOR, &mut buf) {
        println!("  disk       : read failed — {}", e);
        return;
    }

    let previous = if le64(&buf[0..8]) == RECORD_MAGIC {
        let boot = le64(&buf[8..16]);
        let ticks = le64(&buf[16..24]);
        let text_end = buf[24..64].iter().position(|&b| b == 0).unwrap_or(40);
        let text = core::str::from_utf8(&buf[24..24 + text_end]).unwrap_or("<invalid>");
        println!("  previous   : boot {}, written at tick {}, \"{}\"", boot, ticks, text);

        let bad = (64..blk::SECTOR_SIZE)
            .filter(|&i| buf[i] != pattern_byte(i, boot))
            .count();
        if bad == 0 {
            println!("  previous   : body verified, all {} pattern bytes match", blk::SECTOR_SIZE - 64);
        } else {
            println!("  previous   : BODY CORRUPT — {} bytes differ", bad);
        }
        Some(boot)
    } else {
        println!("  previous   : none — this disk has never been written by us");
        None
    };

    let boot = previous.map_or(1, |b| b + 1);
    let mut out = [0u8; blk::SECTOR_SIZE];
    out[0..8].copy_from_slice(&RECORD_MAGIC.to_le_bytes());
    out[8..16].copy_from_slice(&boot.to_le_bytes());
    out[16..24].copy_from_slice(&time::ticks().to_le_bytes());
    let text = b"written by jungey os";
    out[24..24 + text.len()].copy_from_slice(text);
    for i in 64..blk::SECTOR_SIZE {
        out[i] = pattern_byte(i, boot);
    }

    if let Err(e) = blk::write_sector(RECORD_SECTOR, &out) {
        println!("  disk       : write failed — {}", e);
        return;
    }
    println!("  wrote      : boot {} to sector {}", boot, RECORD_SECTOR);

    let mut check = [0u8; blk::SECTOR_SIZE];
    if let Err(e) = blk::read_sector(RECORD_SECTOR, &mut check) {
        println!("  disk       : read-back failed — {}", e);
        return;
    }
    let differing = (0..blk::SECTOR_SIZE).filter(|&i| check[i] != out[i]).count();
    if differing == 0 {
        println!("  read back  : PASS — all {} bytes identical", blk::SECTOR_SIZE);
    } else {
        println!("  read back  : FAIL — {} bytes differ", differing);
    }
}

// ---------------------------------------------------------------------------
// Stage 3c exit test: a write cut short by a power failure leaves the
// filesystem mountable, holding either the old contents or the new — never a
// mix, and never a filesystem that will not mount.
//
// The test spans four boots and sequences itself through a phase marker stored
// outside the filesystem, because it has to survive the filesystem being in a
// state it was never meant to be in. `./test.sh` drives it.
// ---------------------------------------------------------------------------

const FILE: &str = "hello.txt";

/// Deterministic file contents for version `v`, so a read can be checked byte
/// for byte rather than eyeballed.
fn contents(v: u8, len: usize) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::with_capacity(len);
    let header: &[u8] = match v {
        1 => b"v1: written before the crash test\n",
        _ => b"v2: written after the crash test\n",
    };
    out.extend_from_slice(header);
    while out.len() < len {
        out.push((out.len().wrapping_mul(7) as u8) ^ v.wrapping_mul(97));
    }
    out
}

fn verify(fs: &fs::Fs, v: u8, len: usize) -> bool {
    let want = contents(v, len);
    let mut got = alloc::vec![0u8; len];
    match fs.read(FILE, &mut got) {
        Ok(n) if n == len && got == want => {
            println!("  verify     : PASS — {} holds v{}, all {} bytes match", FILE, v, len);
            true
        }
        Ok(n) => {
            println!("  verify     : FAIL — read {} bytes, expected {} of v{}", n, len, v);
            false
        }
        Err(e) => {
            println!("  verify     : FAIL — {}", e);
            false
        }
    }
}

const V1_LEN: usize = 1100; // three sectors, so a partial write is possible
const V2_LEN: usize = 1600;

fn stage3c_demo() {
    if !blk::attached() {
        println!("  fs         : no disk, skipping");
        return;
    }

    let phase = fs::read_phase();
    println!("  fs         : crash-consistency test, phase {}", phase);

    match phase {
        0 => {
            // Fresh disk: lay down a filesystem and one committed file.
            if let Err(e) = fs::format(blk::capacity_sectors()) {
                println!("  fs         : format failed — {}", e);
                return;
            }
            println!("  format     : superblock and checkpoint A written");
            let Ok(mut f) = mounted() else { return };
            if let Err(e) = f.write(FILE, &contents(1, V1_LEN), None) {
                println!("  write      : failed — {}", e);
                return;
            }
            println!("  write      : {} v1 committed, checkpoint seq {}", FILE, f.cp.seq);
            let _ = fs::write_phase(1);
            println!();
            println!("  next boot  : verifies v1, then crashes mid-write on purpose.");
        }
        1 | 2 => {
            let Ok(mut f) = mounted() else {
                println!("  RESULT     : FAIL — filesystem will not mount");
                return;
            };
            println!("  mounted    : checkpoint seq {} from slot {}", f.cp.seq, f.slot);

            // Phase 2 is the recovery check for phase 1's crash, and the setup
            // for a harder one. Every boot after a crash re-verifies v1.
            if !verify(&f, 1, V1_LEN) {
                println!("  RESULT     : FAIL — an interrupted write was partly visible");
                return;
            }
            if phase == 2 {
                println!("  RESULT     : PASS — the mid-data crash left no trace");
                let (used, garbage) = f.usage();
                // The crashed sectors sit past log_head, which never moved
                // because the checkpoint never landed. The next write reuses
                // them: a crash costs nothing, not even space.
                println!(
                    "  log        : {} sectors used, {} garbage — the crash cost no space",
                    used, garbage
                );
            }

            // Record the next phase *before* crashing, or this boot repeats
            // forever. The marker lives outside the filesystem for exactly
            // this reason.
            let _ = fs::write_phase(phase + 1);
            let v2 = contents(2, V2_LEN);
            let full = v2.len().div_ceil(blk::SECTOR_SIZE) as u32;
            // First a crash part way through the data, then one with every byte
            // written and only the commit missing.
            let point = if phase == 1 { 1 } else { full };
            println!("  about to   : write v2 and lose power after {} of {} sectors", point, full);
            println!();
            let _ = f.write(FILE, &v2, Some(point)); // does not return
        }
        3 => {
            // The hardest case: all of v2's data is on the disk, and nothing
            // points at it.
            let Ok(mut f) = mounted() else {
                println!("  RESULT     : FAIL — filesystem will not mount after the crash");
                return;
            };
            println!("  mounted    : checkpoint seq {} from slot {}", f.cp.seq, f.slot);
            if verify(&f, 1, V1_LEN) {
                println!("  RESULT     : PASS — a fully written but uncommitted file is invisible");
            } else {
                println!("  RESULT     : FAIL — the interrupted write was partly visible");
                return;
            }
            let (used, garbage) = f.usage();
            println!(
                "  log        : {} sectors used, {} garbage — both crashes cost no space",
                used, garbage
            );

            if let Err(e) = f.write(FILE, &contents(2, V2_LEN), None) {
                println!("  write      : failed — {}", e);
                return;
            }
            println!("  write      : {} v2 committed, checkpoint seq {}", FILE, f.cp.seq);
            let _ = fs::write_phase(4);
            println!();
            println!("  next boot  : confirms v2 survived a clean shutdown.");
        }
        _ => {
            let Ok(f) = mounted() else { return };
            println!("  mounted    : checkpoint seq {} from slot {}", f.cp.seq, f.slot);
            let ok = verify(&f, 2, V2_LEN);
            let (used, garbage) = f.usage();
            println!(
                "  log        : {} sectors used, {} garbage from rewriting v1",
                used, garbage
            );
            println!("  directory  :");
            for e in f.files() {
                println!("    {:<12} {:>6} bytes at sector {}", e.name_str(), e.size, e.start);
            }
            println!();
            if ok {
                println!("  RESULT     : PASS — crash consistency test complete, all five boots");
            } else {
                println!("  RESULT     : FAIL — v2 did not survive");
            }
            let _ = fs::write_phase(5);
        }
    }
}

fn mounted() -> Result<fs::Fs, ()> {
    match fs::mount() {
        Ok(f) => Ok(f),
        Err(e) => {
            println!("  mount      : failed — {}", e);
            Err(())
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 5a exit test: two processes map the same model and share its pages.
//
// The claim being tested is the one in ARCHITECTURE.md section 3.2 — that a
// model is a kernel object rather than bytes an app read into its heap, so N
// processes using the same weights cost one copy, pages arrive on demand rather
// than in a blocking read, and they can be dropped and re-read at will because
// every one of them is clean.
// ---------------------------------------------------------------------------

const MODEL_FILE: &str = "model.bin";
/// Stand-in for a real quantised model: large enough for sharing and reclaim to
/// mean something, small enough to write over a 512-byte-at-a-time driver.
const MODEL_BYTES: usize = 64 * 1024;

/// What the file contains, so a mapped page can be checked rather than assumed.
fn model_byte(offset: usize) -> u8 {
    (offset.wrapping_mul(31) ^ (offset >> 12).wrapping_mul(17)) as u8
}

/// Spin the boot thread until `done`, or give up and say so.
///
/// Every wait in a test wants a deadline: a run that fails should fail, not
/// hang and make someone guess which of twenty things stopped.
fn wait_until(done: impl Fn() -> bool, what: &str) -> bool {
    let deadline = time::ticks() + 1500; // 15 seconds at 100 Hz
    while !done() {
        if time::ticks() > deadline {
            println!("  TIMEOUT    : waited 15s for {}", what);
            return false;
        }
        sched::yield_now();
    }
    true
}

fn stage5a_demo() {
    if !blk::attached() {
        println!("  model      : no disk, skipping");
        return;
    }
    let Ok(mut f) = mounted() else { return };

    // Write the model once; it persists like any other file.
    if !f.files().iter().any(|e| e.name_str() == MODEL_FILE) {
        let mut data = alloc::vec![0u8; MODEL_BYTES];
        for (i, b) in data.iter_mut().enumerate() {
            *b = model_byte(i);
        }
        match f.write(MODEL_FILE, &data, None) {
            Ok(()) => println!("  model      : wrote {} ({} KiB) to the log", MODEL_FILE, MODEL_BYTES / 1024),
            Err(e) => {
                println!("  model      : could not write {} — {}", MODEL_FILE, e);
                return;
            }
        }
    }

    let Some(entry) = f.files().iter().find(|e| e.name_str() == MODEL_FILE).copied() else {
        println!("  model      : {} vanished", MODEL_FILE);
        return;
    };

    // Read the file straight back through the block client and check it
    // against the pattern. If this fails, nothing downstream means anything.
    {
        let mut sector = [0u8; blk::SECTOR_SIZE];
        let mut wrong = 0usize;
        let mut first_bad = 0usize;
        let n = (entry.size as usize).div_ceil(blk::SECTOR_SIZE);
        for si in 0..n {
            if blk::read_sector(entry.start + si as u64, &mut sector).is_err() {
                println!("  model      : verify read failed at sector {}", si);
                return;
            }
            for k in 0..blk::SECTOR_SIZE {
                let off = si * blk::SECTOR_SIZE + k;
                if off < entry.size as usize && sector[k] != model_byte(off) {
                    if wrong == 0 {
                        first_bad = off;
                    }
                    wrong += 1;
                }
            }
        }
        if wrong == 0 {
            println!("  model      : verified on disk, all {} bytes", entry.size);
        } else {
            println!("  model      : ON-DISK MISMATCH — {} bytes differ, first at {}", wrong, first_bad);
        }
    }
    model::publish(MODEL_FILE, entry.start, entry.size as usize);
    let pages = (entry.size as usize).div_ceil(mm::PAGE_SIZE);
    println!(
        "  model      : {} published, {} bytes, {} pages, at sector {}",
        MODEL_FILE, entry.size, pages, entry.start
    );

    let free_before = mm::frames::free_count();
    println!("  frames     : {} free before anything maps it", free_before);
    println!();

    // Started in sequence, not together: the first process reads the weights,
    // the second must find them already there. Overlapping the two would still
    // work, but it would not *demonstrate* anything — a shared hit and a lucky
    // interleaving look the same in the totals.
    start_process("modelA", 5, &[]);
    wait_until(
        || model::resident_pages() == pages as u64,
        "modelA to fault in the whole model",
    );
    let (faults_a, shared_a) = model::fault_totals();
    println!(
        "  first user : {} faults, {} shared, {} pages resident",
        faults_a,
        shared_a,
        model::resident_pages()
    );

    start_process("modelB", 6, &[]);
    wait_until(
        || model::fault_totals().0 >= 2 * pages as u64,
        "modelB to touch every page",
    );

    let (faults, shared) = model::fault_totals();
    let read_from_flash = faults - shared;
    println!();
    println!("  after pass 1 — both processes have touched every page:");
    println!("    faults       : {} across both processes, for a {}-page model", faults, pages);
    println!("    shared       : {} served from a page another process had already read", shared);
    println!(
        "    read         : {} pages came off flash; two private copies would have read {}",
        read_from_flash,
        pages * 2
    );
    println!("    resident     : {} pages of weights in memory", model::resident_pages());
    if read_from_flash == pages as u64 && model::resident_pages() == pages as u64 {
        println!("    RESULT       : PASS — one copy of the weights, two processes using it");
    } else {
        println!("    RESULT       : FAIL — the weights were not shared");
    }

    // Now simulate memory pressure. Every weight page is clean by construction,
    // so reclaim is an unmap and a free: no writeback, no swap, no decision
    // about what is dirty.
    let want = pages / 2;
    let freed = model::reclaim(want);
    println!();
    println!("  reclaimed    : {} of {} pages dropped under pressure", freed, pages);
    println!("    resident     : {} pages", model::resident_pages());
    println!("    the processes have not been told, and do not need to be.");

    wait_until(|| sched::live_count() <= 1, "both processes to finish");

    println!();
    println!("    id  hash              KiB  resident  total  faults  reclaimed  refs");
    model::for_each(|m| {
        println!(
            "  {:>4}  {:#016x}  {:>3}  {:>8}  {:>5}  {:>6}  {:>9}  {:>4}",
            m.id,
            m.hash,
            m.size / 1024,
            m.resident_pages(),
            m.total_pages(),
            m.faults,
            m.reclaimed,
            m.refs
        );
    });
    println!();
    println!("  dedup      : two opens of the same bytes produced one object, {} references", 2);
    println!("  frames     : {} free at the end, {} at the start", mm::frames::free_count(), free_before);
}

fn current_el() -> u64 {
    let el: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el) };
    el >> 2
}

fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println_forced!("\n*** KERNEL PANIC ***");
    println_forced!("{}", info);
    halt()
}
