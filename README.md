# Jungey OS

A mobile operating system built from scratch, designed around on-device AI as a
scheduled system resource rather than a library.

Not an Android fork. Not a Linux distribution. AArch64 microkernel in Rust,
capability-based, with an inference scheduler and a model store in the kernel's
object model.

Read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the design and
[`docs/ROADMAP.md`](docs/ROADMAP.md) for what is built and what it costs.

## Status

**Stage 0 — it boots.** The kernel comes up on QEMU `virt`, drops from EL2 to
EL1, installs exception vectors, parses the device tree the bootloader hands it,
and manages physical memory.

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
  Jungey OS  v0.1.0  ·  stage 0  ·  aarch64
  ------------------------------------------------
  image      : 0x0040080000..0x00400c7000  (284 KiB)
  exec level : EL1
  dtb        : 0x0048000000
  vectors    : installed at VBAR_EL1
  fdt        : valid, 1048576 bytes
  machine    : linux,dummy-virt
  ram[0]     : 0x0040000000..0x00c0000000  (2048 MiB)
  ram total  : 2048 MiB
  frames     : 523961 free of 523961 (2046 MiB usable)
  alloc test : got 0x0040000000 and 0x0040001000
  alloc test : freed, 523961 frames free
  ------------------------------------------------
  stage 0 complete. idling.
```

Other invocations:

```bash
./run.sh --debug     # debug build (no optimization, assertions on)
./run.sh --gdb       # halt and wait for a debugger on localhost:1234
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
    ├── linker.ld             image layout; loads at 0x4008_0000
    ├── build.rs              wires the linker script into rustc
    └── src/
        ├── boot.s            ARM64 Image header, EL2->EL1, stack, .bss
        ├── vectors.s         16-entry EL1 exception vector table
        ├── main.rs           kernel_main
        ├── uart.rs           PL011 console + print!/println!
        ├── exceptions.rs     VBAR_EL1 setup, ESR decoding
        ├── dtb.rs            flattened device tree reader
        └── mm/
            ├── mod.rs        page constants and alignment helpers
            └── frames.rs     physical frame allocator with reservations
```

## Design notes for contributors

- **Nothing about the board is hardcoded if the device tree knows it.** The UART
  base is still a constant; that is a stage-0 shortcut with an expiry date.
- **Every `unsafe` block states what makes it sound.** The kernel is single-core
  until stage 3; several statics rely on that and say so.
- **The allocator API is the one a buddy allocator can keep.** Stage 1 replaces
  the implementation, not the callers.
