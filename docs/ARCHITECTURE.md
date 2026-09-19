# Jungey OS — Architecture

> Working name. A mobile operating system built from the boot code up, designed
> around on-device inference as a first-class system resource rather than a
> library bolted onto a phone OS.

## 1. The thesis

Android was designed in 2005–2008 for a world where the phone was a thin client:
small heap, one foreground app, the network does the thinking. Every AI feature
since has been retrofitted onto that shape — NNAPI, then vendor NPU SDKs, then
`AICore`, each an accelerator library reached through a HAL, arbitrated by
nothing.

That retrofit leaks in five places, and those five leaks are the entire design
brief for this OS:

| Problem in today's phone OS | What this OS does instead |
|---|---|
| NPU/GPU time is first-come-first-served through a vendor driver; no priority, no preemption, no fairness | **Inference is a scheduled resource** with QoS classes, quotas and preemption, like CPU time |
| A 3 GB model is 3 GB per process; no sharing, no paging strategy, no eviction policy | **Model weights and KV cache are kernel-managed memory objects**: shared, mapped, tiered, evictable |
| Apps are opaque silos; an assistant reaches them by screen-scraping or scripted taps | **Apps publish typed capabilities**; the agent composes them through a schema, not a screenshot |
| Permissions are ambient and app-scoped; an agent acting "as you" inherits everything you granted | **Capability tokens with provenance**: every action carries who asked, why, and under what delegation |
| Battery and thermal policy has no idea inference is running | **Energy is a budget inference jobs bid against**, not a reaction to a thermal trip point |

Nothing here needs a new kernel to *prototype*. But every one of them needs to
be in the kernel's object model to work *reliably*, which is why this is an OS
and not an Android fork.

## 2. Shape of the system

A capability-based microkernel. Drivers, filesystems, network stack and the
inference runtime are userspace servers. The kernel owns four things and nothing
else: address spaces, threads, capabilities, and IPC.

```
 ┌──────────────────────────────────────────────────────────────────────┐
 │  Shell · Apps · Agent surfaces                                       │
 ├──────────────────────────────────────────────────────────────────────┤
 │  Agent runtime      Capability broker      App framework             │
 │  (planner, tools)   (consent, provenance)  (typed intents, UI)       │
 ├──────────────────────────────────────────────────────────────────────┤
 │  Inference server   Model store   Display   Net   Storage   Sensors  │
 │  (graph exec, KV)   (weights)     server    stack  server    server  │
 ├──────────────────────────────────────────────────────────────────────┤
 │  Device servers: NPU · GPU · ISP · modem · PMIC · flash · radios     │
 ├──────────────────────────────────────────────────────────────────────┤
 │  MICROKERNEL — address spaces, threads, capabilities, IPC, scheduling│
 └──────────────────────────────────────────────────────────────────────┘
```

**Why a microkernel, honestly.** It costs IPC round trips that a monolith
doesn't pay, and Linux is faster at almost everything today. It is chosen anyway
because the central claim of this OS — that an AI agent can act on your behalf
without acting *as* you — requires unforgeable, delegatable, revocable authority
at the lowest level. A monolithic kernel with an ambient-authority syscall table
cannot express that; you end up re-implementing capabilities in userspace and
they leak. seL4 and Fuchsia's Zircon are the precedents.

**Why Rust.** No GC, no runtime, real AArch64 bare-metal support, and the class
of bug that has historically dominated kernel CVEs is a compile error.

## 3. The five subsystems that make it different

### 3.1 Tensor scheduler — inference as scheduled time

The kernel schedules *compute contexts* on accelerators the way it schedules
threads on CPUs. An inference job carries:

- a **QoS class**: `interactive` (voice, keyboard, camera assist — bounded
  latency), `foreground` (user is waiting), `background` (indexing, summarizing),
  `opportunistic` (runs only on charger, cold silicon and idle NPU);
- a **deadline** where one exists, so the scheduler can admit or refuse rather
  than silently miss it;
- an **energy budget** in joules, charged against the app and visible to the user.

Preemption on NPUs is hardware-dependent and mostly absent, so the scheduler
works at *graph-segment* granularity: models are split at layer boundaries into
segments with bounded runtime, and a higher-priority job can land between
segments. This is the same compromise GPU compositors make, and it is what turns
"my assistant makes the keyboard stutter" into a solved scheduling problem.

### 3.2 Model store — weights as a memory object

A model is a kernel object, not a file an app read into its heap:

- **Content-addressed and shared.** Two apps using the same 4-bit Llama share one
  set of physical pages, refcounted. Today they'd hold two copies.
- **Mapped, not loaded.** Weights are demand-paged read-only from flash with
  prefetch hinted by the graph's layer order, so start-up is a page-fault storm
  that finishes in tens of milliseconds, not a multi-second `read()`.
- **Reclaimable.** Under pressure, clean weight pages are dropped first — they're
  re-readable from flash. This is a page-cache tier that understands what it holds.
- **KV cache is a first-class, evictable tier.** Per-session, sized by the tensor
  scheduler, spillable to flash, with an eviction policy that knows a conversation's
  prefix is more valuable than its middle. On a 12 GB phone this is the difference
  between two live models and one.

### 3.3 Capability broker — the agent authority problem

Every resource in the system is named by an unforgeable capability. An app never
"has the camera permission"; it holds a capability to a specific camera, possibly
attenuated (640×480, no audio, expires in 30 s).

When an agent acts for you, each request carries a **delegation chain**: user →
agent → tool → resource, each link attenuating rather than widening. The broker
can answer questions no phone OS can answer today:

- "What did the assistant actually do last Tuesday, and on whose authority?"
- "Revoke everything derived from the consent I gave that app" — one edge cut,
  the whole subtree dies.
- "This capability was delegated to a model whose output I didn't read" — flagged.

Agent actions are recorded in a tamper-evident log, and mutating actions run
through transactional intents so that "undo what it just did" is a real operation.

### 3.4 Typed capability apps — replacing screen-scraping

Android's Intents are a loose string-keyed contract; agents work around them by
driving accessibility APIs, which is fragile and a security disaster. Here an app
declares a machine-readable interface — typed arguments, effects, idempotency,
cost, and whether it needs confirmation — and the agent plans over that surface.
The UI becomes one renderer of those capabilities rather than the only door in.

### 3.5 Energy-aware scheduling

Inference is the new dominant load. Thermal and battery policy is a first-class
input to the tensor scheduler rather than a governor reacting after the fact:
jobs declare a budget, the scheduler admits what fits in the remaining thermal
headroom, and `opportunistic` work is the elastic band that absorbs the rest.

## 4. Kernel object model (target)

| Object | Purpose |
|---|---|
| `AddressSpace` | Page tables, VMA list, reclaim policy |
| `Thread` | Register context, scheduling parameters |
| `Endpoint` | Synchronous IPC rendezvous |
| `Channel` | Asynchronous typed message ring |
| `MemoryObject` | Physical page set: anonymous, file-backed, or *weights* |
| `TensorContext` | An accelerator execution context, schedulable |
| `Capability` | A rights-attenuated, revocable reference to any of the above |
| `Irq` | Delivered to a userspace driver as a message |

IPC is a typed, schema-described message over shared-memory rings, with
zero-copy handoff of `MemoryObject` handles — so passing a 2 GB tensor between
the inference server and an app is a capability transfer, not a copy.

## 5. Hardware plan

1. **QEMU `virt`** — current target. Full visibility, fast iteration, no blobs.
2. **Rockchip RK3588 board** (Orange Pi 5 / Radxa Rock 5) — 6 TOPS NPU, mainline
   Linux support to crib from, open enough schematics. This is where the tensor
   scheduler meets real silicon.
3. **Raspberry Pi 5** — secondary, for display/input/USB bring-up without an NPU.
4. **A phone, eventually** — a Pixel or a PinePhone, via `libhybris`-style reuse
   of Android's vendor blobs for modem and GPU. Writing a baseband stack is not
   on this roadmap and should not be on anyone's.

## 6. Non-goals

- **Binary compatibility with Android or Linux.** No `libc` ABI promise, no APKs.
  Compatibility is a later, optional userspace personality.
- **Writing a cellular baseband.** Regulatory and practical dead end.
- **Beating Linux on throughput.** This OS bets on latency, predictability and
  authority, not raw syscall speed.
- **Being the phone you ship to customers next year.** See `ROADMAP.md` for what
  is honest.

## 7. Current state

Stage 0 is implemented and boots (`os/kernel`). See `ROADMAP.md` for what each
stage delivers and what it costs.
