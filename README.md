# Jungey OS

A mobile operating system built from scratch, designed around on-device AI as a
scheduled system resource rather than a library.

Not an Android fork. Not a Linux distribution. AArch64 microkernel in Rust,
capability-based, with an inference scheduler and a model store in the kernel's
object model.

Read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the design and
[`docs/ROADMAP.md`](docs/ROADMAP.md) for what is built and what it costs.

## Status

**Stage 3b — it has a disk.** A virtio-blk driver over the modern virtio-mmio
transport: feature negotiation, a split virtqueue, physically addressed DMA
buffers, and completion interrupts through the GIC. A sector written on one boot
is read back on the next.

**Stage 3a — it runs on every core.** Secondary cores are started through PSCI,
per-CPU state lives on `TPIDR_EL1`, and threads — kernel and user alike —
migrate freely between cores off one run queue. Locks are genuinely contended
now, and the kernel powers the machine off when its demos finish.

**Stage 2 — it runs processes.** On top of stage 1's MMU, scheduler and GICv3,
the kernel now loads ELF images into isolated address spaces, runs them at EL0,
and serves them seven system calls. A process's entire authority is the
capabilities in its table: there is no ambient permission, no global name it can
guess, and no way to widen a capability it was handed — only to narrow it.
Revoking a capability kills everything derived from it, wherever it ended up.

## Run it

Requirements: a Rust toolchain and `qemu-system-aarch64`. The kernel's build
script builds `os/user` and embeds the result, so one command builds both.

```bash
rustup target add aarch64-unknown-none-softfloat
rustup component add llvm-tools
sudo apt install qemu-system-arm     # or your platform's equivalent

cd os
./run.sh
```

Quit QEMU with `Ctrl-A` then `X`.

Expected output (abridged — hardware discovery and the capability demo come
first; see `docs/ROADMAP.md` for those):

```
  cpus       : 4 in the device tree, psci via hvc
  cpu 1      : online (mpidr 0x1)
  cpu 2      : online (mpidr 0x2)
  cpu 3      : online (mpidr 0x3)
  smp        : 4 of 4 cores online
  ----------------------------------------------------------
  [receiver] pid 0 wrote its pid to 0x0000000000402000, reads back 0
  [sender  ] pid 1 wrote its pid to 0x0000000000402000, reads back 1
  [sender  ] send ok
  [sender  ] refused, as it should be: EPERM (capability lacks the right)
  [receiver] got: "hello from the sender"
  [intruder] denied: EBADCAP (no such capability)

  !! pid Some(3) fault: data abort, lower EL at far 0xffff000040080000
  !! esr 0x000000009200000d — terminating the process

  [kernel  ] revoking root capability #1
  [kernel  ] 2 derived capabilities died with it
  [receiver] dead: EREVOKED (capability was revoked)
  [sender  ] dead: EREVOKED (capability was revoked)
  heap check after stage 2 : ok — 3 free blocks, 1044352 bytes free
  ----------------------------------------------------------
  8 threads on 4 cores, each taking one lock 20000 times

  shared counter : 160000 of 160000 expected
  lock           : PASS — no update lost under cross-core contention
  contended on   : cores 0123
  elapsed        : 9 ticks (90 ms)

  thread          state  slices  cores
  boot         runnable      17  0123
  idle0        runnable       3  0
  ...
  receiver     finished       3  02
  w2           finished       5  0123
  w6           finished       5  0123

  cpu   switches   timer ticks
  0           21            89
  1           15            89
  2           18            89
  3           18            88

  idle wakeups   : 298
  irqs           : 0 spurious, 0 unclaimed
  heap check after stage 3a : ok — 4 free blocks, 1042432 bytes free
  ----------------------------------------------------------
  virtio     : probing 32 mmio transports
  disk       : virtio-blk, 131072 sectors (64 MiB), intid 79, queue at 0x40256000
  previous   : boot 2, written at tick 90, "written by jungey os"
  previous   : body verified, all 448 pattern bytes match
  wrote      : boot 3 to sector 64
  read back  : PASS — all 512 bytes identical
  completion : 3 interrupts from the device
  ----------------------------------------------------------
  stage 3b complete.
  powering off via psci.
```

Read that output as a set of claims, each of which fails loudly if broken:

**Capabilities (stage 2)**

- **Authority is only what you hold.** The message crosses only because each
  side was handed a capability.
- **Rights narrow, never widen.** The sender holds SEND and is refused RECV.
- **An index is not a name.** The intruder guesses slot 0 and gets `EBADCAP`.
- **Address spaces are separate.** Both processes write to `0x402000` and read
  back their own pid.
- **Revocation cuts the subtree.** One call at the root; both derived
  capabilities die; neither process was notified, they just find out.
- The trespasser reads a kernel address from EL0, is killed for it, and nothing
  else notices.

**Multiprocessing (stage 3a)**

- **Every core does work.** The switch and tick counts per CPU are within one
  of each other.
- **Threads are not pinned.** The `cores` column shows each worker running on
  several cores, user processes included.
- **Locks hold under real concurrency.** 160,000 increments from 8 threads on
  4 cores, none lost. A broken lock reports the exact shortfall.
- **The heap is intact.** Checked between stages, because corruption surfaces
  as a fault somewhere else entirely, thousands of instructions later.

**Storage (stage 3b)**

- **The disk remembers.** Boot twice and `previous` reads what the last boot
  wrote. `./run.sh --fresh` starts from an empty image.
- **The whole sector is checked**, not just a header: the body carries a
  pattern derived from the boot number, so stale data cannot pass as fresh.

The kernel powers off through PSCI when it finishes, so `./run.sh` returns
rather than idling forever.

Other invocations:

```bash
./run.sh --debug     # debug build (no optimization, assertions on)
./run.sh --gdb       # halt and wait for a debugger on localhost:1234
./run.sh --fault     # dereference null on purpose, to see the fault report
./run.sh --fresh     # start from an empty disk image
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
├── user/                     userspace, built and embedded by the kernel build
│   ├── linker.ld             loaded at 4 MiB, segments grouped by permission
│   └── src/
│       ├── main.rs           the four test roles
│       └── sys.rs            syscall stubs — the whole kernel interface
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
        ├── time.rs           generic timer, per-core 100 Hz tick
        ├── smp.rs            PSCI bring-up, per-CPU state on TPIDR_EL1
        ├── virtio.rs         virtio-mmio transport and virtio-blk driver
        ├── dtb.rs            flattened device tree reader
        ├── cap.rs            capabilities: minting, derivation, revocation
        ├── ipc.rs            channels and message queues
        ├── proc.rs           processes: address space + capability table
        ├── elf.rs            ELF64 loader
        ├── syscall.rs        the seven system calls
        ├── sched/
        │   ├── mod.rs        threads, run queue, round-robin, sleep, idle
        │   └── switch.s      context switch and new-thread trampoline
        └── mm/
            ├── mod.rs        page constants, PHYS_OFFSET, phys<->virt
            ├── frames.rs     physical frame allocator with reservations
            ├── heap.rs       first-fit kernel heap, with an integrity checker
            ├── paging.rs     page tables, address spaces, permissions
            └── uaccess.rs    copying across the user/kernel boundary
```

## Memory layout

```
  TTBR1   VA 0xFFFF_0000_0000_0000 + PA   linear map of all physical memory
          VA 0xFFFF_0000_4008_0000        kernel image (PA 0x4008_0000)

  TTBR0   VA 0x0000_0000_0040_0000        user text   (r-x at EL0)
          VA 0x0000_0000_0040_1000        user rodata (r-- at EL0)
          VA 0x0000_0000_0040_2000        user data   (rw- at EL0)
          VA 0x0000_0000_6FFF_C000        user stack, 16 KiB
```

Every user mapping sets PXN, so a kernel bug that jumps into user memory faults
instead of executing whatever the process put there. A kernel thread runs with
TTBR0 pointing at an empty table rather than the last process's, so a stray low
access is a translation fault and not someone else's data.

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
- **Anything the hardware keeps one copy of belongs in the exception frame.**
  `ELR_EL1`, `SPSR_EL1` and `SP_EL0` are all per-core, not per-thread. The
  scheduler switches threads from inside exception handlers, so leaving any of
  them in a system register corrupts the thread being left — intermittently,
  which is the worst way to find out.
- **The kernel never dereferences a user pointer.** Every access goes through
  `uaccess`, which translates via that process's own page tables first.
- **A thread stays claimed across a context switch.** It is released by the
  thread the core runs next, once `cpu_switch_to` has actually saved its
  context. Releasing it any earlier lets another core restore a context that is
  still being written.
- **A thread is created stopped and started explicitly.** On four cores, a
  thread that is visible is a thread that is already running — before whatever
  was going to be attached to it has been.
- **A driver never waits only on an interrupt.** Completion is polled with a
  deadline; the interrupt is still taken, acknowledged and counted. An interrupt
  that does not arrive should cost an error, not the machine.
