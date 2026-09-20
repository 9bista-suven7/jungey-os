#!/usr/bin/env bash
# Build and boot the Jungey OS kernel under QEMU.
#
#   ./run.sh            release build, serial on stdout (Ctrl-A X to quit)
#   ./run.sh --debug    debug build
#   ./run.sh --gdb      halt and wait for a debugger on :1234
#   ./run.sh --fault    end the demo with a deliberate null dereference
#   ./run.sh --fresh    start from an empty disk image
#   ./run.sh --tamper   record the wrong image digest, to watch the boot refuse
set -euo pipefail

# Fail with something useful if the toolchain is missing, rather than letting
# the shell report "cargo: command not found". Resolved before any cd, so it
# works whatever directory you invoke this from.
JUNGEY_ROOT="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tools/preflight.sh
. "$JUNGEY_ROOT/tools/preflight.sh"
preflight yes

cd "$(dirname "$0")/kernel"

PROFILE=release
FEATURES=()
QEMU_EXTRA=()
FRESH=0

for arg in "$@"; do
    case "$arg" in
        --debug) PROFILE=debug ;;
        --gdb)   QEMU_EXTRA+=(-s -S) ;;
        --fault) FEATURES+=(--features fault-demo) ;;
        --fresh) FRESH=1 ;;
        --tamper) export JUNGEY_TAMPER=1 ;;
        -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

CARGO_FLAGS=()
[ "$PROFILE" = release ] && CARGO_FLAGS+=(--release)
[ ${#FEATURES[@]} -gt 0 ] && CARGO_FLAGS+=("${FEATURES[@]}")

cargo build ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}

ELF="target/aarch64-unknown-none-softfloat/$PROFILE/jkernel"
BIN="target/jkernel.bin"

# Flatten to a raw image so QEMU (and U-Boot's `booti`, and EDK2) honour the
# ARM64 Image header in boot.s and enter us with the DTB pointer in x0.
# A plain ELF is treated as bare-metal firmware and gets no device tree.
OBJCOPY="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/llvm-objcopy"
"$OBJCOPY" -O binary "$ELF" "$BIN"

# A disk that survives between runs, which is the point of the stage 3b test:
# boot twice and the kernel should read back what the previous boot wrote.
#
# force-legacy=false is required: QEMU's virtio-mmio transports default to the
# legacy (version 1) interface for compatibility, and this kernel implements the
# modern one. QEMU also fills the transport slots from the last one downwards,
# so the driver probes all 32 rather than assuming slot 0.
DISK="disk.img"
[ "$FRESH" = 1 ] && rm -f "$DISK"
[ -f "$DISK" ] || truncate -s 64M "$DISK"

# gic-version=3 because the kernel drives a GICv3, the controller every AArch64
# SoC worth targeting ships. QEMU still defaults `virt` to v2 for compatibility.
exec qemu-system-aarch64 \
    -M virt,gic-version=3 \
    -cpu cortex-a72 \
    -smp 4 \
    -m 2G \
    -nographic \
    -kernel "$BIN" \
    -global virtio-mmio.force-legacy=false \
    -drive file="$DISK",if=none,format=raw,id=hd0 \
    -device virtio-blk-device,drive=hd0 \
    ${QEMU_EXTRA[@]+"${QEMU_EXTRA[@]}"}
