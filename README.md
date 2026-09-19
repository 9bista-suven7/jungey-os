# Jungey OS

A mobile operating system built from scratch, designed around on-device AI as a
scheduled system resource rather than a library.

Not an Android fork. Not a Linux distribution. AArch64 microkernel in Rust,
capability-based, with an inference scheduler and a model store in the kernel's
object model.

Read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the design and
[`docs/ROADMAP.md`](docs/ROADMAP.md) for what is built and what it costs.

## Status

**Stage 1 — it schedules.** The kernel boots on QEMU `virt`, drops from EL2 to
EL1, turns the MMU on and relocates itself into the higher half, discovers its
hardware from the device tree, allocates physical frames and heap, takes
interrupts through a GICv3, and preemptively round-robins kernel threads off the
generic timer.

## Run it

Requirements: a Rust toolchain and `qemu-system-aarch64`.

```bash
rustup target add aarch64-unknown-none-softfloat
rustup component add llvm-tools
sudo apt install qemu-system-arm     # or your platform's equivalent

cd os
./run.sh
```

Quit QEMU with `Ctrl-A` then `X`.

Expected output:

```
  Jungey OS  v0.2.0  ·  stage 1  ·  aarch64
  ----------------------------------------------------------
  image      : 0xffff000040080000..0xffff0000400cf000  (316 KiB)
  phys       : 0x0040080000..0x00400cf000
  exec level : EL1
  mmu        : on, linear map at 0xffff000000000000
  vectors    : installed at VBAR_EL1
  dtb        : 0x0048000000 phys, 1048576 bytes
  machine    : linux,dummy-virt
  console    : pl011 at 0x0009000000 (from dtb)
  ram[0]     : 0x0040000000..0x00c0000000  (2048 MiB)
  frames     : 523954 free of 523954 (2046 MiB usable)
  heap       : 1024 KiB
  gic        : v3 at 0x0008000000/0x00080a0000, 288 interrupt lines
  sched      : round-robin, thread 0 is 'boot'
  timer      : cntv at 62500000 Hz, tick 100 Hz, intid 27
  irq        : unmasked
  ----------------------------------------------------------
  spawning 3 worker threads; timer preempts every 10 ms

  [alpha] tick  42    29937386 iterations
  [ beta] tick  43    29872709 iterations
  [gamma] tick  44    30126893 iterations
  ...
  all workers done. sleeping the boot thread for 50 ticks —
  with an empty run queue the core must sit in WFI.
  woke after 50 ticks; idle ran 49 times while nothing was runnable

  thread          state  slices
  boot         runnable      53
  idle         runnable       1
  alpha        finished      51
  beta         finished      51
  gamma        finished      51

  uptime     : 2000 ms (200 ticks)
  irqs       : 0 spurious, 0 unclaimed
  ----------------------------------------------------------
  stage 1 complete. handing the core to idle.
```

Equal slice counts across the three workers is the round-robin fairness check;
49 idle wakeups during a 50-tick sleep is the proof the core actually stopped
rather than spinning an empty run queue.

Other invocations:

```bash
./run.sh --debug     # debug build (no optimization, assertions on)
./run.sh --gdb       # halt and wait for a debugger on localhost:1234
./run.sh --fault     # dereference null on purpose, to see the fault report
```

`--fault` should print, and then panic:

```
*** EXCEPTION: EL1h sync ***
  esr_el1 = 0x0000000096000004  (data abort, same EL)
  far_el1 = 0x0000000000000000
  fault   : at 0x0000000000000000, translation fault — nothing mapped there
```

To attach a debugger:

```bash
gdb-multiarch os/kernel/target/aarch64-unknown-none-softfloat/release/jkernel \
    -ex 'target remote :1234'
```

## Layout

```
os/
├── run.sh                    build + boot under QEMU
├── docs/
│   ├── ARCHITECTURE.md       the design and why it is shaped this way
│   └── ROADMAP.md            stages 0-7, exit tests, honest costs
└── kernel/
    ├── linker.ld             image layout; linked high, loaded at 0x4008_0000
    ├── build.rs              wires the linker script into rustc
    └── src/
        ├── boot.s            Image header, EL2->EL1, page tables, MMU on
        ├── vectors.s         16-entry EL1 exception vector table
        ├── main.rs           kernel_main and the stage 1 demo
        ├── uart.rs           PL011 console + print!/println!
        ├── sync.rs           IRQ-masking spinlock
        ├── exceptions.rs     VBAR_EL1 setup, ESR and fault-status decoding
        ├── irq.rs            interrupt dispatch
        ├── gic.rs            GICv3 distributor, redistributor, CPU interface
        ├── time.rs           generic timer, 100 Hz scheduler tick
        ├── dtb.rs            flattened device tree reader
        ├── sched/
        │   ├── mod.rs        threads, run queue, round-robin, sleep, idle
        │   └── switch.s      context switch and new-thread trampoline
        └── mm/
            ├── mod.rs        page constants, PHYS_OFFSET, phys<->virt
            ├── frames.rs     physical frame allocator with reservations
            └── heap.rs       first-fit kernel heap behind GlobalAlloc
```

## Memory layout

```
  VA 0xFFFF_0000_0000_0000 + PA        linear map of all physical memory
  VA 0xFFFF_0000_4008_0000             kernel image (PA 0x4008_0000)
  VA 0x0000_0000_0000_0000 .. low      unmapped: TCR_EL1.EPD0 disables TTBR0
```

The kernel is *linked* high and *loaded* low, so everything in `boot.s` before
the MMU comes up addresses symbols PC-relatively (`adrp`), which yields physical
addresses at runtime. The one deliberate link-time absolute is the branch into
the higher half.

## Design notes for contributors

- **Nothing about the board is hardcoded if the device tree knows it.** The UART
  and the GIC are both found by `compatible` string. The one remaining constant
  is the PL011 base used for the very first `println!`, before the DTB is read.
- **Every `unsafe` block states what makes it sound.** The kernel is single-core
  until stage 3; several statics rely on that and say so.
- **The allocator API is the one a buddy allocator can keep.** A later stage
  replaces the implementation, not the callers.
- **Locks mask interrupts.** One core runs kernel code today, but IRQ handlers
  already preempt it, so a lock that does not mask would deadlock the core
  against itself.
