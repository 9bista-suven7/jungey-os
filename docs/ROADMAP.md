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

## Stage 3 — SMP, drivers, storage  🔶 *in progress*

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

### 3c — Filesystem  ⬜

A log-structured filesystem, because flash, and because crash consistency for
the agent's action log is a stage 6 requirement that has to be designed in here.

**Exit test:** a file written and then power-cut mid-write leaves the filesystem
mountable, with either the old contents or the new, never a mix.

### 3d — Userspace drivers  ⬜

The driver framework: MMIO regions and IRQs as capabilities, so a driver is an
ordinary process holding a `Device` capability. Move virtio-blk out of the
kernel to prove the framework carries a real driver.

**Exit test:** a file written through a userspace filesystem server survives a
hard reset, and the kernel contains no block-device code.

*Cost: 3–5 months for the stage. 3a took days.*

---

## Stage 4 — Graphics, input, the shell

Display server on DRM/KMS-equivalent, GPU bring-up (Panfrost/Freedreno as
reference), a compositor, touch input, fonts and text layout, and a first shell.
This is where it stops being a console and starts being a device.

**Exit test:** touch a button on a real panel on real hardware and something
happens at 60 fps.

*Cost: 4–8 months. Text layout and font rendering alone are a month you won't
have budgeted for.*

---

## Stage 5 — The inference stack  ← *the reason this project exists*

The tensor scheduler, the model store, quantized kernels (start CPU NEON/SME,
then the NPU), a graph executor with segment-level preemption, KV-cache tiering,
and the energy budget accounting. First real model running on device under
scheduler control.

**Exit test:** a 3B model generates tokens while a second `interactive` job
preempts it mid-generation and meets a 50 ms deadline, with both jobs' joule
consumption reported accurately.

*Cost: 6–12 months. The scheduler is the novel part; the kernels are a known
quantity you can borrow from llama.cpp/MLC.*

---

## Stage 6 — Agent runtime and typed apps

The capability broker, the delegation-chain log, transactional intents with undo,
the typed-capability app manifest format, the planner, and an SDK. The app model
is the product; everything below is plumbing that makes it trustworthy.

**Exit test:** the agent completes a three-app task it was never scripted for,
you can read the full delegation chain afterwards, and you can undo it.

*Cost: 6–12 months, and it never really ends.*

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
