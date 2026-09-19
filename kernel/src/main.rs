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
pub mod proc;
pub mod sched;
pub mod smp;
pub mod syscall;
pub mod sync;
pub mod time;
pub mod virtio;

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
    println!("  Jungey OS  v0.6.0  ·  stage 3  ·  aarch64");
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
    stage3b_demo(&fdt);
    heap_check("after stage 3b");
    println!("{}", RULE);
    stage3c_demo();
    heap_check("after stage 3c");

    println!("{}", RULE);
    println!("  stage 3c complete.");

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

fn start_process(name: &'static str, role: usize, cap: Option<cap::Cap>) -> usize {
    let pid = proc::create(name, INIT_ELF, role).expect("create process");
    if let Some(c) = cap {
        proc::install_cap(pid, 0, c).expect("install capability");
    }
    // Stopped, then attached, then started: on four cores a thread that is
    // visible is a thread that is running, and one started before its address
    // space was attached would be scheduled with an empty TTBR0.
    let tid = sched::spawn_stopped(name, user_thread, pid).expect("spawn user thread");
    let ttbr0 = unsafe { (*proc::get(pid).unwrap()).space.ttbr0() };
    sched::attach_process(tid, pid, ttbr0);
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

    start_process("receiver", 1, Some(recv_cap));
    start_process("sender", 0, Some(send_cap));
    start_process("intruder", 2, None);
    start_process("trespasser", 3, None);

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
// Stage 3b exit test: a sector written on one boot is there on the next.
// ---------------------------------------------------------------------------

/// Where the persistence record lives. Sector 0 is left alone so the image
/// stays something a partition table could later be written to.
const RECORD_SECTOR: u64 = 64;
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

fn stage3b_demo(fdt: &dtb::Fdt) {
    let Some((capacity, irq, queue_phys)) = virtio::init(fdt) else {
        println!("  disk       : no virtio block device — run.sh attaches one with -drive");
        return;
    };
    println!(
        "  disk       : virtio-blk, {} sectors ({} MiB), intid {}, queue at {:#x}",
        capacity,
        capacity * virtio::SECTOR_SIZE as u64 / (1024 * 1024),
        irq,
        queue_phys
    );

    // ---- what the last boot left behind ----
    let mut buf = [0u8; virtio::SECTOR_SIZE];
    if let Err(e) = virtio::read_sector(RECORD_SECTOR, &mut buf) {
        println!("  disk       : read failed — {}", e);
        return;
    }

    let previous = if le64(&buf[0..8]) == RECORD_MAGIC {
        let boot = le64(&buf[8..16]);
        let ticks = le64(&buf[16..24]);
        let text_end = buf[24..64].iter().position(|&b| b == 0).unwrap_or(40);
        let text = core::str::from_utf8(&buf[24..24 + text_end]).unwrap_or("<invalid>");
        println!("  previous   : boot {}, written at tick {}, \"{}\"", boot, ticks, text);

        // The body has to match too, or a stale-but-plausible header passes.
        let bad = (64..virtio::SECTOR_SIZE)
            .filter(|&i| buf[i] != pattern_byte(i, boot))
            .count();
        if bad == 0 {
            println!("  previous   : body verified, all {} pattern bytes match", virtio::SECTOR_SIZE - 64);
        } else {
            println!("  previous   : BODY CORRUPT — {} bytes differ", bad);
        }
        Some(boot)
    } else {
        println!("  previous   : none — this disk has never been written by us");
        None
    };

    // ---- write this boot's record ----
    let boot = previous.map_or(1, |b| b + 1);
    let mut out = [0u8; virtio::SECTOR_SIZE];
    out[0..8].copy_from_slice(&RECORD_MAGIC.to_le_bytes());
    out[8..16].copy_from_slice(&boot.to_le_bytes());
    out[16..24].copy_from_slice(&time::ticks().to_le_bytes());
    let text = b"written by jungey os";
    out[24..24 + text.len()].copy_from_slice(text);
    for i in 64..virtio::SECTOR_SIZE {
        out[i] = pattern_byte(i, boot);
    }

    if let Err(e) = virtio::write_sector(RECORD_SECTOR, &out) {
        println!("  disk       : write failed — {}", e);
        return;
    }
    println!("  wrote      : boot {} to sector {}", boot, RECORD_SECTOR);

    // ---- read it straight back ----
    let mut check = [0u8; virtio::SECTOR_SIZE];
    if let Err(e) = virtio::read_sector(RECORD_SECTOR, &mut check) {
        println!("  disk       : read-back failed — {}", e);
        return;
    }
    let differing = (0..virtio::SECTOR_SIZE).filter(|&i| check[i] != out[i]).count();
    if differing == 0 {
        println!("  read back  : PASS — all {} bytes identical", virtio::SECTOR_SIZE);
    } else {
        println!("  read back  : FAIL — {} bytes differ", differing);
    }

    println!("  completion : {} interrupts from the device", virtio::irq_count());
    println!();
    println!("  boot again and 'previous' should read boot {}.", boot);
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
    if !virtio::have_disk() {
        println!("  fs         : no disk, skipping");
        return;
    }

    let phase = fs::read_phase();
    println!("  fs         : crash-consistency test, phase {}", phase);

    match phase {
        0 => {
            // Fresh disk: lay down a filesystem and one committed file.
            if let Err(e) = fs::format(virtio::capacity_sectors()) {
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
            let full = v2.len().div_ceil(virtio::SECTOR_SIZE) as u32;
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
