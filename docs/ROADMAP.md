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

## Stage 2 — Userspace and capabilities

EL0 tasks, ELF loading, syscall path via `SVC`, address-space isolation, the
capability table, `Endpoint` and `Channel` IPC, and the first userspace server
(a RAM disk). Per-process capability derivation and revocation.

**Exit test:** two userspace processes exchange a message they could not have
exchanged without an explicitly granted capability, and revoking the parent
capability kills the channel for both.

*Cost: 2–4 months. The capability model is the project's thesis — get it wrong
here and everything above inherits the mistake.*

---

## Stage 3 — SMP, drivers, storage

Secondary core bring-up (PSCI), per-CPU run queues, spinlocks and RCU-ish
read paths, userspace driver framework with IRQ-as-message, a real block driver
(virtio-blk, then UFS/eMMC), and a filesystem — most likely a log-structured one,
because flash and because crash consistency for the agent's action log matters.

**Exit test:** four cores running, a file written through a userspace filesystem
server survives a hard reset, and `fsck` finds nothing.

*Cost: 3–5 months.*

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
