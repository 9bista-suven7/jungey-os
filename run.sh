#!/usr/bin/env bash
# Build and boot the Jungey OS kernel under QEMU.
#
#   ./run.sh            release build, serial on stdout (Ctrl-A X to quit)
#   ./run.sh --debug    debug build
#   ./run.sh --gdb      halt and wait for a debugger on :1234
set -euo pipefail

cd "$(dirname "$0")/kernel"

PROFILE=release
CARGO_FLAGS=(--release)
QEMU_EXTRA=()

for arg in "$@"; do
    case "$arg" in
        --debug) PROFILE=debug; CARGO_FLAGS=() ;;
        --gdb)   QEMU_EXTRA+=(-s -S) ;;
        -h|--help) sed -n '2,6p' "$0"; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

cargo build "${CARGO_FLAGS[@]}"

ELF="target/aarch64-unknown-none-softfloat/$PROFILE/jkernel"
BIN="target/jkernel.bin"

# Flatten to a raw image so QEMU (and U-Boot's `booti`, and EDK2) honour the
# ARM64 Image header in boot.s and enter us with the DTB pointer in x0.
# A plain ELF is treated as bare-metal firmware and gets no device tree.
OBJCOPY="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/llvm-objcopy"
"$OBJCOPY" -O binary "$ELF" "$BIN"

exec qemu-system-aarch64 \
    -M virt \
    -cpu cortex-a72 \
    -smp 4 \
    -m 2G \
    -nographic \
    -kernel "$BIN" \
    "${QEMU_EXTRA[@]}"
