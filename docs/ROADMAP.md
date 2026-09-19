# Jungey OS — Roadmap

Honest scoping. Effort figures assume one determined person working evenings and
weekends, with an AI pair. Halve them for full-time; double them for a first
kernel. Nothing here is padded to look impressive and nothing is shortened to
look achievable.

Each stage has an **exit test** — a thing that either runs or doesn't. No stage
is "done" because the code was written.

---

## Stage 0 — It boots  ✅ *done*

Bare-metal AArch64: Image header, EL2→EL1 drop, stack, `.bss`, UART console,
exception vectors, device tree parsing, physical frame allocator.

**Exit test:** `./run.sh` prints the banner, the machine model read from the DTB,
the real RAM map, and a frame allocation round-trip. ✅

*Cost: days.*

---

## Stage 1 — Virtual memory and threads  ✅ *done*

MMU on (TTBR0/TTBR1 split, 4 KiB granule, 48-bit VA) with the kernel relinked
into the higher half at `PHYS_OFFSET`; the low half is then unmapped outright
via `TCR_EL1.EPD0`, so a null dereference faults instead of poking MMIO. Kernel
heap, GICv3 driver, generic timer at 100 Hz, per-thread kernel stacks,
preemptive round-robin scheduling, sleeping threads, and an idle thread that
stops the core in WFI.

**Exit test:** three kernel threads round-robin on a timer tick, a deliberate
null dereference produces a page fault report naming the faulting address, and
the core enters WFI when nothing is runnable. ✅

Measured on QEMU `virt`:

```
  thread          state  slices        alpha/beta/gamma get 51 slices each
  boot         runnable      53        over 150 ticks — round-robin is fair
  idle         runnable       1
  alpha        finished      51        idle ran 49 times during a 50-tick
  beta         finished      51        sleep: the core really stopped
  gamma        finished      51
  irqs       : 0 spurious, 0 unclaimed
```

```
*** EXCEPTION: EL1h sync ***
  esr_el1 = 0x0000000096000004  (data abort, same EL)
  far_el1 = 0x0000000000000000
  fault   : at 0x0000000000000000, translation fault — nothing mapped there
```

**Two deviations from the original plan, both deliberate:**

- *No buddy allocator.* The plan called for one. Kernel allocation at this stage
  is low-rate and long-lived, and a buddy allocator answers fragmentation
  pressure that does not exist yet. The frame allocator grew `alloc_contiguous`
  for multi-page runs, and the heap is a first-fit free list with coalescing.
  Revisit when stage 5's weight pages and KV cache make fragmentation real —
  the callers do not change when the implementation does.
- *Whole-RAM linear map with 1 GiB blocks*, rather than fine-grained kernel
  mappings. Four L1 descriptors map everything; per-page kernel permissions
  (W^X for the kernel image) land with the stage 2 page-table code.

*Cost: was estimated at 1–2 months. This is the stage where most hobby kernels
are abandoned; MMU bugs are silent and the debugger is a UART.*

---

## Stage 2 — Userspace and capabilities  ✅ *done*

EL0 processes, per-process page tables at 4 KiB granularity, an ELF64 loader,
the `SVC` syscall path, capability tables with derivation and subtree
revocation, and channel IPC with blocking receive.

**Exit test:** two userspace processes exchange a message they could not have
exchanged without an explicitly granted capability, and revoking the parent
capability kills the channel for both. ✅

Measured on QEMU `virt`:

```
  channel 0 created; root capability #1 (send+recv)
  derived #2 (send) and #3 (recv) from #1

  [receiver] pid 0 wrote its pid to 0x0000000000402000, reads back 0
  [sender  ] pid 1 wrote its pid to 0x0000000000402000, reads back 1
  [sender  ] send ok
  [sender  ] refused, as it should be: EPERM (capability lacks the right)
  [receiver] got: "hello from the sender"
  [intruder] denied: EBADCAP (no such capability)

  !! pid Some(3) fault: data abort, lower EL at far 0xffff000040080000

  [kernel  ] revoking root capability #1
  [kernel  ] 2 derived capabilities died with it

  [sender  ] dead: EREVOKED (capability was revoked)
  [receiver] dead: EREVOKED (capability was revoked)
```

Five properties, each with its own line in that output:

| Property | How it shows |
|---|---|
| Authority is only what you hold | receiver and sender exchange a message; neither could without its capability |
| Rights attenuate, never widen | sender holds SEND, is refused RECV with `EPERM` |
| No ambient authority | intruder guesses slot 0, gets `EBADCAP` — an index is not a name |
| Address spaces are separate | both processes write to `0x402000` and read back their own pid |
| Revocation kills the subtree | one cut at the root, both derived capabilities die, neither process was told |

The trespasser process reads a kernel address from EL0 and is killed; every
other process carries on, which is the other half of the isolation claim.

**Two bugs worth recording, both silent and both intermittent:**

- *`SP_EL0` was not saved across a context switch.* There is one `SP_EL0` per
  core. Preempt a user thread inside a syscall, return to a different one, and
  it resumes on the other process's stack pointer — reads a stale return
  address, and branches to it. Failed roughly 2 runs in 10 before the exception
  frame grew to carry it, which is exactly the frequency that gets a bug
  shipped. `ELR_EL1` and `SPSR_EL1` had the same problem in stage 1; `SP_EL0`
  is the one that is easy to forget because nothing in the kernel reads it.
- *`enter_user` was interruptible between writing `ELR_EL1` and `eret`.*
  Exception entry overwrites both `ELR_EL1` and `SPSR_EL1` with the interrupted
  kernel state, so a timer tick in that window made the `eret` jump to a kernel
  address at EL0. It now masks interrupts for the sequence; `eret` restores
  `PSTATE` from `SPSR_EL1`, so the process is still preemptible from its first
  instruction.

Both were found by running the same build twelve times, not by reading the code.

**Deviations from the plan, deliberate:**

- *No userspace RAM disk server.* The init image is embedded in the kernel and
  loaded from memory. A filesystem server needs a block driver, which is stage 3.
- *One thread per process, and no reaping.* Exited processes keep their address
  space and kernel stack. Both are stage 3 work, and both want the refcounted
  handles that SMP needs anyway.
- *`Endpoint` (synchronous rendezvous) is not implemented*, only `Channel`
  (queued, asynchronous). Nothing in the exit test needs rendezvous semantics,
  and the capability model is the same either way.

*Cost: was estimated at 2–4 months. The capability model is the project's
thesis — get it wrong here and everything above inherits the mistake.*

---

## Stage 3 — SMP, drivers, storage  ✅ *done*

Broken into four parts, each with its own exit test, because the whole stage is
too big to land or verify at once.

### 3a — SMP  ✅ *done*

Secondary cores started through PSCI, per-CPU state on `TPIDR_EL1`, one global
run queue with threads free to migrate, per-core GICv3 redistributors and
generic timers, and a heap integrity checker.

**Exit test:** every core runs work, threads migrate between cores, and a lock
held across cores loses no updates. ✅

```
  cpu 1      : online (mpidr 0x1)
  cpu 2      : online (mpidr 0x2)
  cpu 3      : online (mpidr 0x3)
  smp        : 4 of 4 cores online

  8 threads on 4 cores, each taking one lock 20000 times
  shared counter : 160000 of 160000 expected
  lock           : PASS — no update lost under cross-core contention
  contended on   : cores 0123

  thread          state  slices  cores
  w2           finished       5  0123
  w6           finished       5  0123
  receiver     finished       3  02        <- user processes migrate too

  cpu   switches   timer ticks
  0           21            89
  1           15            89
  2           18            89
  3           18            88
```

25 consecutive clean runs on 4 cores, and it still works on `-smp 1`.

**Two concurrency bugs, both found by running it, not by reading it:**

- *A thread was released before its context was saved.* `schedule` cleared
  `on_cpu` under the lock and only then called `cpu_switch_to`. In that window
  another core could pick the same thread up and restore a context that was
  still being written — two cores running one thread on one stack. The symptom
  was a wild pointer fault or a hang, in about 3 runs in 8. The fix is the one
  Linux calls `finish_task_switch`: the thread stays claimed across the switch
  and is released by whichever thread the core runs next, which is the first
  moment the save is complete.
- *`spawn` published a thread before `attach_process` bound it.* On one core a
  newly spawned thread waits its turn; on four, another core runs it
  immediately — with no pid and an empty TTBR0, so its syscalls returned
  `EINVAL` and its next context switch unmapped its own code. Threads are now
  created stopped and started explicitly, which is why `State::New` exists.

Neither is visible on one core. Both are the whole reason this stage is
sequenced before the driver work rather than after it.

**Deviation:** *one global run queue, not per-CPU queues.* Per-CPU queues buy
lock throughput this kernel has no way to measure a need for, and they cost the
property that makes this version reviewable — that there is exactly one place a
thread's state can change. Revisit when a profile says the scheduler lock is
hot.

### 3b — Block driver  ✅ *done*

virtio-mmio transport and a virtio-blk driver: feature negotiation, a split
virtqueue with descriptor chains, DMA buffers addressed physically, and
completion interrupts arriving as SPIs through the GIC.

**Exit test:** write a sector, read it back on a fresh boot from the same disk
image. ✅

```
  virtio     : probing 32 mmio transports
  disk       : virtio-blk, 131072 sectors (64 MiB), intid 79, queue at 0x40256000
  previous   : boot 2, written at tick 90, "written by jungey os"
  previous   : body verified, all 448 pattern bytes match
  wrote      : boot 3 to sector 64
  read back  : PASS — all 512 bytes identical
  completion : 3 interrupts from the device
```

20 consecutive boots against one disk image, the boot counter incrementing by
one each time and the sector body verified against a value-dependent pattern —
so a stale-but-plausible header cannot pass.

**Two things QEMU makes you get right:**

- *The transports default to legacy.* Every one of the 32 slots reports version
  1 until `-global virtio-mmio.force-legacy=false`. This driver implements the
  modern interface and refuses version 1 rather than half-supporting it.
- *Slots fill from the last one downwards.* The block device lands in slot 31.
  A driver that assumes slot 0 finds nothing, which is a good reason to probe.

**Deviations:**

- *Completion is polled, not awaited.* The interrupt fires, is acknowledged and
  is counted, but the wait loop yields and checks the used ring rather than
  blocking on the interrupt, with a two-second deadline. A driver that can only
  be woken by an interrupt hangs the machine when the interrupt does not come.
  Interrupt-driven completion belongs in 3d, where an IRQ becomes a message to a
  driver process and a lost one is that process's problem.
- *One request in flight, one sector at a time.* The queue is 8 deep and the
  driver uses one slot of it. Batching is worth doing when something is waiting
  on throughput; nothing is yet.
- *DMA assumes a coherent device.* True under QEMU and on most ARM SoCs with
  virtio. Real non-coherent hardware needs the queue and buffers mapped
  non-cacheable, or explicit cache maintenance around every request.

### 3c — Filesystem  ✅ *done*

JLFS: an append-only log with two checkpoint slots. Data is written to the log
first, then a single-sector checkpoint switches the filesystem's root. A crash
either loses the checkpoint — and with it every trace of the unfinished write —
or lands after it, with the data already durable. There is no order in which a
reader sees half of one.

**Exit test:** a file written and then power-cut mid-write leaves the filesystem
mountable, with either the old contents or the new, never a mix. ✅

The test spans five boots and sequences itself through a marker stored *outside*
the filesystem, because it has to survive the filesystem being in a state it was
never meant to be in. `./test.sh` drives it; power is cut for real, with PSCI
`SYSTEM_OFF`, not simulated.

```
boot 2   verify     : PASS — hello.txt holds v1, all 1100 bytes match
         crash      : power cut after 1 of 4 data sectors

boot 3   mounted    : checkpoint seq 2 from slot 1
         verify     : PASS — hello.txt holds v1, all 1100 bytes match
         RESULT     : PASS — the mid-data crash left no trace
         crash      : power cut with all 4 data sectors written, checkpoint skipped

boot 4   mounted    : checkpoint seq 2 from slot 1
         RESULT     : PASS — a fully written but uncommitted file is invisible
         write      : hello.txt v2 committed, checkpoint seq 3

boot 5   verify     : PASS — hello.txt holds v2, all 1600 bytes match
         RESULT     : PASS — crash consistency test complete, all five boots
```

Two crash points, and the second is the one that matters: every byte of the new
file is on the disk and the filesystem still reports the old contents, because
nothing points at the new ones. A filesystem that merely looks tidy fails that
case.

The crashes cost no space, either. The orphaned sectors sit past `log_head`,
which never advanced because the checkpoint never landed, so the next write
reuses them.

**Assumption, stated rather than buried:** a single sector write is atomic — it
lands whole or not at all. Every journalling filesystem assumes this. The
checkpoint CRC catches the case where the hardware breaks its promise, which
turns silent corruption into a refusal to mount that slot.

**Deviations:**

- *Flat directory, 8 files, one sector.* The log and the atomic root swap are
  what this stage is about; a B-tree is a later problem and does not change the
  consistency argument.
- *No cleaner.* A rewrite orphans the old sectors and nothing reclaims them.
  The other half of a log-structured filesystem, and one that wants a real
  workload to be tuned against rather than a guess.
- *Whole-file writes only.* No partial updates, no append, no seek.

### 3d — Userspace drivers  ✅ *done*

Hardware is named the way everything else is. `Mmio`, `Irq` and `Dma` are
capability objects, and three syscalls act on them: map a device's registers,
map a DMA region and learn its physical address, wait for an interrupt. The
virtio-blk driver moved out of the kernel and became an ordinary process.

**Exit test:** a file written through a userspace driver survives a hard reset,
and the kernel contains no block-device code. ✅

```
  bus        : block device in a virtio transport at 0xa003e00, intid 79, version 2
  driver     : blkdrv is pid 4, holding 5 capabilities:
    slot 0     #4 channel (recv)
    slot 1     #5 channel (send)
    slot 2     #6 mmio (map)
    slot 3     #7 irq (irq)
    slot 4     #8 dma (map)
  [blkdrv  ] attached in userspace: 131072 sectors, dma at 0x0000000040265000
  disk       : 131072 sectors (64 MiB), driven entirely from userspace
  ...
  [blkdrv  ] shutting down after 15 device interrupts
  driver     : 17 requests served by the userspace driver
```

Those five capabilities are the driver's entire authority. It has no argument it
could pass to reach a second device: `map_device` takes *where in its own
address space* to put the registers, never *which* registers — that comes from
the capability. The whole of stage 3b and 3c now runs through it, including the
power-cut crash tests.

The kernel keeps exactly three virtio register offsets, in `devices.rs`, to read
what kind of device is in each transport slot. That is bus enumeration, not a
driver: no virtqueue, no descriptors, no block protocol, and nothing that has to
change when the disk becomes UFS.

**Three bugs, each only reachable once a driver ran on a different core from its
client:**

- *A lost wakeup in `recv`.* The receiver checked for a message, found none, and
  then marked itself blocked. A sender on another core landing in that window
  woke a thread that was still runnable — the wake did nothing, and the receiver
  then blocked forever with its message already queued. Blocking is now two
  steps: `prepare_block`, re-check, then `block` or `cancel_block`. This is
  Linux's `prepare_to_wait` discipline, and it exists for exactly this reason.
- *Syscalls ran with interrupts masked.* Taking an exception masks them in
  hardware, and nothing unmasked them again. A user thread waiting inside a
  syscall therefore stopped its core's timer: no preemption, no tick, and every
  deadline in the system became infinite — the machine deadlocked with three
  cores idle. Userspace had interrupts enabled and the syscall now does too,
  re-masking before the vector restores registers.
- *Sub-page MMIO windows cannot be isolated.* virtio-mmio spaces its transports
  0x200 apart, so eight share a 4 KiB page and the mapping refused to align.
  `map_device` now maps the containing page and returns the offset, which makes
  the compromise visible rather than hiding it: a driver holding one of these
  capabilities can reach its seven neighbours' registers. The real fixes are
  hardware that spaces devices a page apart, an SMMU, or a trusted shim. Linux
  and VFIO hit the same wall.

**Deviations:**

- *The filesystem is still in the kernel.* It is a client of the driver, not of
  a device — `fs.rs` does not know what kind of storage is underneath or that
  the code driving it runs outside the kernel. Moving it out is a lift-and-shift
  of a file with no new mechanism behind it, and it is not what this stage was
  testing.
- *`irq_wait` polls the sequence counter* rather than sleeping on a wait queue
  with a timer-backed timeout. It takes a mandatory timeout, so a line that
  never asserts costs an error rather than a wedged driver.

*Cost: 3–5 months for the stage. 3a took days.*

---

## Stage 4 — Graphics, input, the shell  ◐ *4a done, the rest is hardware*

Display server, GPU bring-up (Panfrost/Freedreno as reference), a compositor,
touch input, fonts and text layout, and a first shell. This is where it stops
being a console and starts being a device.

**Exit test:** touch a button on a real panel on real hardware and something
happens at 60 fps.

### 4a — A pointer, a compositor, and two applications  ✅ *done*

The part of stage 4 that does not need a panel or a GPU: who owns a window, who
a tap belongs to, and whether an application can reach past what it holds. It
is worth doing first because it is the part the rest has to be built on, and
because getting it wrong is invisible until much later.

A third userspace driver (virtio-input, an absolute pointer — which is what a
touchscreen looks like to software) holds **four** capabilities rather than
five: it has nothing to receive, so it is given no way to. The display server
keeps a retained command list for the background, composites windows over it,
and transfers only the damaged rectangle. Two applications hold **two**
capabilities each — send window operations, receive taps on their own windows —
and that is the entirety of what they can do.

**Exit test:** a tap goes to exactly one window, the same pixel goes to a
different application once the one underneath is raised, a tap outside every
window goes nowhere, and an application cannot touch a window it did not
create. ✅

```
  bus        : input device in a virtio transport at 0xa003a00, intid 77
  input      : inputdrv is pid 10, holding 4 capabilities:
    slot 0     #16 channel (send)
    slot 1     #17 mmio (map)
    slot 2     #18 irq (irq)
    slot 3     #19 dma (map)
  [inputdrv] attached in userspace: "QEMU Virtio Tablet", dma at 0x405eb000
  [inputdrv] absolute pointer, x 0..32767, y 0..32767 -> 480x960
  ui         : shell is pid 11 (2 capabilities), notes is pid 12 (2 capabilities)
  ui         : 2 windows open, notes on top of the shell where they overlap

  [gpudrv  ] tap at 239,370 -> pid 12 window 1 at 169,40
  [notes   ] row 0 "GROCERIES" is now on
  [gpudrv  ] tap at 239,190 -> pid 11 window 0 at 209,40
  [shell   ] row 0 "RUN A MODEL" is now on
  [gpudrv  ] tap at 239,370 -> pid 11 window 0 at 209,220
  [shell   ] row 5 "SLEEP" is now on
  [gpudrv  ] tap at 239,699 hit no window

  taps       : 3 routed to a window, 1 landed on nothing
  ownership  : 1 operation(s) named a window the sender does not own, all refused
  [gpudrv  ] smallest transfer 85k pixels, 18% of the screen
  RESULT     : PASS — every tap reached exactly one window, the same point
               went to a different application once the one underneath was
               raised, a tap outside every window reached nobody, and an
               application could not touch a window it did not create
```

The taps are real: `tools/uitest.sh` injects them through QEMU's monitor, so
they arrive at the guest's virtio-input device exactly as a finger's would, and
everything past the device registers is the system under test. `./sim.sh --gui`
lets you tap it yourself.

**The interesting line is the third tap.** It is the same pixel as the first and
it goes to a different process, because tapping a window raises it. A
compositor that remembered who asked last, or delivered to everyone and let the
applications sort it out, would pass the first two taps and fail that one.

**Windows are keyed by (owner, id), and the owner is the pid the kernel
recorded at `send` time** — not a field in the message. That is what makes the
ownership check trustworthy rather than polite: `notes` asks to move window 0,
which belongs to the shell, and finds nothing of its own by that name. The
kernel's own sender id is `usize::MAX`, which no process can hold, so the
server can tell a background command from an application's window operation
without trusting anything it was told. This was briefly broken: the sender was
copied out to userspace as a `u32`, which truncated `usize::MAX` into a value a
process could in principle hold, and every command from the kernel was rejected
as malformed. The symptom was three taps vanishing in silence — a good
reminder that an identity check fails quietly in both directions.

**Deviations, and the first is the one that matters:**

- ***The compositor runs inside the display driver's process, not its own.***
  Sending each frame to a separate compositor would mean copying 1.8 MB through
  a message queue per frame, because there is no way yet for two processes to
  share a buffer. The split is the right design and it waits on shared memory
  objects; putting it in now would mean either a wrong number or a fake one.
- *Damage is one bounding box*, not a list of rectangles, so two changes far
  apart over-report. The number printed is what was actually transferred, so
  the over-reporting is visible rather than hidden.
- *No GPU acceleration*: every pixel is written by the CPU. There is no NPU and
  no GPU under QEMU, and Panfrost-equivalent bring-up is a stage of its own.
- *No text layout.* An 8x8 bitmap font, uppercase folded, no kerning, no
  shaping, no scripts other than ASCII. Real text layout is the month nobody
  budgets for, and it is still ahead.
- *The title bar's height is part of the protocol* because the server draws it
  and the client does the hit-testing inside its own window. A cleaner design
  sends the content origin with the event.
- *A window's title is what the server draws and the only decoration there is.*
  No resize, no close button, no drag.

**What is left of stage 4 is the hardware half**, and it is the expensive half:
a display server against real DRM/KMS-equivalent hardware, a GPU driver, a
compositor that hits 60 fps on a panel rather than an emulator, text layout, and
a shell somebody would want to use.

*Cost: 4–8 months for the rest. Text layout and font rendering alone are a month
you won't have budgeted for.*

---

## Stage 5 — The inference stack  ✅ *done* ← *the reason this project exists*

Split the way stage 3 was, because the whole thing is too big to verify at once.

### 5a — Model store  ✅ *done*

Weights as a kernel object: content-addressed so identical bytes are one
object however they were named, demand-paged so a model starts in milliseconds
rather than after a blocking read, and reclaimable because every resident page
is clean by construction.

**Exit test:** two processes map the same model; the weights are read from flash
once, not twice; dropping half the pages under pressure is invisible to both,
and re-reading them returns the same bytes. ✅

```
  model      : verified on disk, all 65536 bytes
  model      : model.bin published, 65536 bytes, 16 pages, at sector 11

  after pass 1 — both processes have touched every page:
    faults       : 32 across both processes, for a 16-page model
    shared       : 16 served from a page another process had already read
    read         : 16 pages came off flash; two private copies would have read 32
    resident     : 16 pages of weights in memory
    RESULT       : PASS — one copy of the weights, two processes using it

  reclaimed    : 8 of 16 pages dropped under pressure
  [modelA  ] pass 2: 128 bytes checked, 0 wrong after 8 pages were reclaimed
  [modelB  ] pass 2: 128 bytes checked, 0 wrong after 8 pages were reclaimed
```

The kernel gained real demand paging to do it: a translation fault from EL0 is
now a request for a page rather than a death sentence, answered from a
per-process list of regions. A page being read is marked `Loading`, so two cores
faulting the same page do not both read it — on a real model that is a wasted
megabyte, not a wasted page.

**Three bugs, all of them the same shape — a race that only exists because a
driver, a client and two model users now run at once on four cores:**

- *A lost wakeup that survived one fix.* The receiver checked its channel, found
  it empty, and then marked itself blocked; a sender landing in that window woke
  a thread that was not yet waiting. The first fix — mark blocked, re-check,
  then block — narrowed the window but did not close it, and in any case was
  written and never wired up, which a careful reading of the file would have
  caught sooner than three rounds of instrumentation did. The real fix is
  structural: the receiver marks itself blocked *while holding the channel
  lock*, so a sender must either enqueue first and be seen, or find the receiver
  already waiting. One stall in five boots became none in fifteen.
- *Block requests were not serialised.* Two threads sent on the same channel and
  polled the same reply channel, taking each other's answers. It did not look
  like a lock bug; it looked like the disk returning wrong data.
- *A timed-out request left its reply behind.* The next request took that as its
  own and every answer afterwards was one behind. Replies now carry the tag of
  the request they answer, and a mismatched one is discarded.

And one that was purely my own: the stage 3b persistence record lived at sector
64, which is *inside* the filesystem's log. The test spent a boot corrupting the
model file it had just written.

**Deviations:**

- *The model is a 64 KiB file of a known pattern*, not real weights. What is
  being tested is sharing, demand paging and reclaim, none of which care what
  the bytes mean.
- *Reclaim is newest-first*, not a working-set or clock policy. Choosing well
  needs a real access pattern to choose against.
- *No KV cache tier yet* — that is 5c.

### 5b — Tensor scheduler  ✅ *done*

Accelerator time as a scheduled resource. Four QoS classes, earliest-deadline
first within a class, preemption at segment boundaries, admission control
against a measured segment cost, and per-job latency and energy accounting.

**Exit test:** a long background job is preempted mid-run by an interactive one
that meets a 50 ms deadline, and both jobs' costs are attributed correctly. ✅

```
   job  class          segs  done  latency  deadline   met  preempt   energy  state
     1  foreground       4     4       1ms       0ms     -        0      3 mJ  done
     2  opportunistic   20    20      89ms       0ms     -        0     17 mJ  done
     3  background     150   150      80ms       0ms     -        1    175 mJ  done
     4  interactive      5     5       2ms      50ms   yes        0      4 mJ  done
     5  interactive    400     0       0ms      20ms     -        0      0 mJ  refused

  latency    : interactive waited 2 ms; the background job it interrupted ran for 80 ms
               without preemption the interactive job queues behind that and misses by 30 ms
  preempted  : 1 times — the background job gave way
  refused    : 1 job(s) whose deadline could not be met
  opportunist: 1 of 20 segments while the device was wanted; the rest took 6 ms once it was not
```

That table is the whole argument in one place:

- **2 ms against 80 ms.** The interactive job waited one segment, not one job.
  On a device that serves requests in arrival order it waits behind the whole
  background job and misses its deadline by 30 ms. This is the keyboard stutter,
  and it is a scheduling problem with a scheduling answer.
- **Refused, not missed.** 400 segments in 20 ms cannot be done, so it is
  declined at submission. An application that is told no can fall back to a
  smaller model; one that is told yes and then missed can only stutter.
- **Opportunistic is elastic.** One segment while anything else wanted the
  device, nineteen in 6 ms once nothing did.
- **Costs land on the job that incurred them**, including the preemption count,
  which is the thing an aggregate can never tell you.

**What is real here and what is modelled**, because the distinction matters more
than the numbers: the scheduling, admission control, preemption and accounting
are real code making real decisions. The accelerator is not — there is no NPU in
QEMU, so a segment is executed by a CPU loop and its cost is *measured* (≈360 µs
here) rather than assumed, with the running average feeding admission decisions.
Energy is that measured time times a fixed 2.5 W figure: a model, labelled as
one. What is being tested is what the scheduler decides and when, and that does
not change when the executor becomes silicon.

**Deviations:**

- *One device, one context.* Real parts have several engines and a DSP besides.
  The queue is per-device already; making it per-engine is mechanical.
- *Segments are uniform.* A real graph's segments vary by layer, and the cost
  estimate should be per-segment rather than a device-wide average.
- *No thermal input yet* — that is 5d, where energy stops being an output and
  becomes an admission constraint.

### 5c — KV cache tier  ✅ *done*

A per-session cache under a fixed budget, spilled to a disk region the
filesystem formally reserves, with an eviction policy that scores a block on
both age and position.

**Exit test:** more sessions than fit; the right ones are evicted; a spilled
session restores and continues correctly. ✅

```
  filled     : 4 sessions x 20 blocks = 80 asked for, budget is 64
    resident   : 64 blocks — the budget held
    evicted    : 16 blocks spilled to flash
    of those   : 0 were prefix blocks

    session  blocks  resident  spilled  prefix resident
          1      20         4       16          4/4      <- oldest, lost its middle
          2      20        20        0          4/4
          3      20        20        0          4/4
          4      20        20        0          4/4

  read back  : 327680 bytes checked across 80 blocks
    integrity  : PASS — every byte survived the round trip to flash

  pressure   : 20 more conversations, 4 blocks each
    prefix out : 32 prefix blocks evicted (0 before)
```

Three things that distinguish this from a library allocating its own memory:

- **A dropped block is written out first.** Unlike a weight page it cannot be
  regenerated by re-reading a file, and that single difference is why the model
  store can drop a page for free and this cannot.
- **Position counts as much as age.** A conversation's prefix is re-read every
  turn; its middle usually is not. Evicting purely by recency throws away the
  prefix of an idle session and keeps the middle of a busy one, which is exactly
  backwards. Here the oldest session lost all sixteen of its middle blocks and
  none of its prefix.
- **The prefix discount is a discount, not a pin.** With twenty more
  conversations open, prefixes alone exceed the budget and thirty-two of them
  are evicted. A policy that could never evict a prefix would deadlock the cache
  instead of degrading.

**The spill region is carved out in the superblock**, and the filesystem refuses
to grow its log into it. The alternative — assuming the log would never reach
that far — is the same assumption that had an earlier version of this project
quietly overwriting its own model file.

**One bug, and a familiar one:** the first version chose a victim, wrote it out
and finished, all while holding the cache lock. The write goes to a driver
process, which means yielding, and the lock masks interrupts — so the core
stopped taking timer interrupts and the machine hung on the first eviction.
Eviction is now three steps: choose under the lock, write without it, finish
under it again, with a `Spilling` state so a reader waits rather than racing.
That is the third time this shape of mistake has appeared, and the second time
in the same form as `blk`.

**Deviations:**

- *Blocks are a fixed 4 KiB*, not sized to a model's head dimensions.
- *The spill allocator is a bump pointer* — spilled blocks are never reused, so
  a long-running system would exhaust the region. It wants the same cleaner the
  filesystem wants.
- *No compression.* Quantising a spilled KV block is the obvious next lever and
  is orthogonal to the tiering.

### 5d — Energy budgets  ✅ *done*

Temperature as an input to admission rather than a reason to throttle after the
fact, and an energy cap a job can be held to.

**Exit test:** opportunistic work is admitted only within budget and gives way
when headroom disappears. ✅

```
  thermal    : 11.3C above ambient, throttle at 15.0C, critical at 45.0C

  cold device:
    opportunistic work     admitted as job 5

  warm device (past the throttle point, short of critical):
    opportunistic work     REFUSED — device at 33.0C above ambient, limit 15.0C
    background work        REFUSED — device at 33.0C above ambient, limit 15.0C
    interactive work       admitted as job 9

  critical device:
    interactive work       REFUSED — device at 54.1C above ambient, limit 45.0C

  cooling    : idling until the device drops below the throttle point
    opportunistic work     admitted as job 13

  energy cap : 40 segments of background work, capped at 4000 uJ
    stopped after 4 of 40 segments, having spent 4141 uJ of 4000
```

A phone has no fan. Sustained accelerator work heats the package until something
gives, and on a conventional device what gives is *everything*: the governor
notices late and throttles the whole SoC, including the thing the user is
waiting for. Making temperature an admission input means the work that yields is
**chosen**. Between the throttle point and critical — a deliberately wide band —
the device still does everything that matters to whoever is holding it, and only
the work nobody is waiting for is turned away.

**An energy budget is a cap, not a promise.** "Do as much as four millijoules
buys" is a reasonable thing for background work to ask, so a cap smaller than
the work is admitted and binds later. A *deadline* is a request to be finished
and can be answered honestly at submission, which is why that one is a refusal.
The cap overshoots by one segment — it stops once spent, and segments are not
divisible — and the report says so rather than rounding it away.

**Deviations:**

- *The thermal model is a single lumped temperature* that rises with work and
  decays towards ambient. Real behaviour needs the part's own characterisation,
  several sensors and a hysteresis policy. What is under test is what the
  scheduler does with the number.
- *Power is a fixed 2.5 W figure*, so energy is time in disguise. A real part
  draws differently per operator and per clock.
- *No per-app energy attribution over time* — joules land on jobs, not on the
  application that keeps submitting them.

*Cost: 6–12 months for the stage. The scheduler is the novel part; the kernels
are a known quantity you can borrow from llama.cpp/MLC.*

---

## Stage 6 — Agent runtime and typed apps  ✅ *done*

The app model is the product; everything under it is plumbing that exists to
make the app model trustworthy. An assistant that can act on your behalf is only
tolerable if three things are true at once: it can only compose what applications
chose to publish, every action it takes is attributable to a delegation chain you
can read, and you can put things back.

**Exit test:** the agent completes a task it was never scripted for, you can read
the full delegation chain afterwards, and you can undo it. ✅

```
  audit      : log at sector 112640, 0 records already recorded

  operations published by applications:
    name             provider   effect      confirm  args
    notes.read       notes      read-only   no       file
    notes.rewrite    notes      mutates     no       file, text
    journal.append   journal    mutates     yes      file, text
    message.send     messages   external    yes      to, text

  the assistant is asked to summarise today's notes into the journal.
  it composes 2 published operations, none of them written for this task:
    notes.read from notes (read-only)
    notes.rewrite from notes (mutates)

  done       : 2 steps, both files rewritten: true

  the capability every one of those actions ran under, traced back:
    -> #11  journal-writer   append the summary           rights send
       #10  assistant        summarise today's notes      rights send+recv
       #9   you              these are your notes         rights all

  action log : 2 records
    #0   tick 269   cap #11  journal.append     journal.txt
    #1   tick 270   cap #11  notes.rewrite      notes.txt
    chain      : intact

  undo       : 2 steps reversed, 0 could not be
    both files are byte-for-byte what they were before: true
    nothing was copied — the old contents were still in the log

  tamper     : record #0 altered on disk
    verification: chain breaks at #0

  RESULT     : PASS — the assistant composed published operations it was not
               written for, every action is attributable to a delegation chain
               three links deep, the whole task was undone byte for byte, and
               altering the record of it was detected
```

**Three pieces, and each answers a question the others cannot.**

*Typed operations* (`intent.rs`) are what an application publishes: a name, who
provides it, what arguments it takes, whether it only reads, mutates local state
or reaches the outside world, and whether a human should confirm it. Nothing is
callable that was not published. The agent in the demo composes `notes.read` and
`notes.rewrite` — two operations written by the notes app for its own use, with
no knowledge of this task — and it can do that only because they are typed. The
effect type is the useful part: `message.send` is *external*, which is the
difference between an agent that made a mess you can clean up and one that sent
something to another person.

*Delegation provenance* (`cap.rs`) makes every capability carry who holds it and
what for, and a pointer to the capability it was derived from. The chain in the
demo is three links deep — you, the assistant, the journal writer — and each link
holds strictly less than the one above it: `all` becomes `send+recv` becomes
`send`. That was already true of stage 2's capabilities; what is new is that the
chain is *readable after the fact*, so "why was this file written?" has an answer
that is not a guess.

*The action log* (`audit.rs`) is 128-byte records in a reserved region of the
disk, each one hashed together with the hash of the record before it. Every
action names the capability it ran under, so the log and the chain join up. The
demo deliberately corrupts a record and shows verification naming the exact
record that broke — because a tamper-evident log that has never been shown to
detect tampering is a claim, not a property.

**Undo is not a copy.** `intent::record` is called *before* each write, not
after, and what it stores is the file's directory entry — a pointer into the log
at the sectors the old contents still occupy. JLFS never overwrites, so undoing a
task means writing the old entry back. The cost is a directory entry per step,
and it is why the demo can say "byte for byte" rather than "close enough". A step
recorded after its write could not be undone if the machine stopped in between;
the ordering is the whole mechanism.

**Deviations, and one of them is large:**

- ***There is no planner, and that is deliberate.*** The demo's agent selects
  operations by effect type from the registry; it does not decide *what to do*.
  The thing that turns "summarise today's notes" into a sequence of calls is a
  language model, and there is no NPU in QEMU and no model to run on it. What is
  under test is everything that has to be true *around* the planner — the type
  system it plans against, the authority its calls run under, the record they
  leave, and the undo. Bolting a planner onto that is a smaller job than building
  it; building it first and adding the safety afterwards is how this goes wrong.
- *The action log is tamper-evident, not tamper-proof.* It detects modification;
  it cannot prevent it, and an attacker who can write the disk can rewrite the
  whole chain from the altered record forward. Making that hard needs a hardware
  root of trust — a monotonic counter or a sealed key the kernel cannot read —
  which is stage 7 work.
- *The hash is FNV-1a*, not a cryptographic one, and is trivially forgeable by
  anyone trying. It is there to make the structure and the verification path
  real; the function is a one-line substitution once there is a reason to pay
  for it.
- *`confirm` is a field nobody reads yet.* Operations declare whether a human
  should approve them; there is no UI to ask, because stage 4 does not exist.
- *Applications are in-kernel registrations*, not processes with manifests. The
  manifest format and the SDK are what turns this into something a third party
  can ship, and neither is written.

*Cost: 6–12 months for the planner, the manifest format and the SDK, and it never
really ends.*

---

## Stage 7 — A device you carry

Power management that reaches multi-day standby, suspend/resume, secure and
verified boot with your own keys, OTA updates with A/B slots and rollback,
modem integration, and the long unglamorous tail of thermals and reliability.

**Exit test:** it is your only phone for two weeks.

*Cost: 12+ months, and this is the stage that kills projects that survived
everything else.*

---

## Reality check

**Where this actually stands.** Stages 0, 1, 2, 3 (a–d), 4a, 5 (a–d) and 6 are
built, and each one's exit test runs — `./test.sh` is sixty-odd assertions
across six boots, and it fails rather than hangs. The rest of stage 4 and all of
stage 7 are not, and neither is a matter of another few commits: stage 4 needs a
real GPU and a real panel, and stage 7 needs a real phone, a modem, a key store
and the patience to carry one as a daily driver. Both are listed here as years
because they are years. The honest summary is that the *software* story —
inference as a scheduled resource, authority you can trace, actions you can
undo, a tap that goes to exactly one window — is real and tested under QEMU, and
the *device* story has not started.

Stages 0–3 are a genuinely achievable solo project and teach more than any
course. Stage 5 is where the idea becomes *worth something* — and it is reachable
without stages 4 and 7, on a dev board with no screen.

**The highest-leverage move is to reorder:** do 0 → 1 → 2 → 3 → **5**, prove the
tensor scheduler and model store on an RK3588 with a serial console, and only
then decide whether stages 4 and 7 are worth the years they cost. If the
inference scheduling story is real, that is the thing worth shipping — possibly
as a contribution to an existing kernel rather than a whole phone.

And it is worth saying plainly: Android is ~15 years and thousands of engineers.
Matching its breadth solo is not on the table. Beating it on one axis that
matters — how the machine treats inference — is.
