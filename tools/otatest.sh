#!/usr/bin/env bash
# Stage 7c exit test: six power cycles, one A/B update that never comes up,
# and one that does.
#
# Every interesting property of an A/B update scheme is about what survives
# the machine stopping, so the only honest way to test it is to stop the
# machine. This boots the same kernel six times on one disk and lets the
# control block on that disk sequence the run.
#
#   ./tools/otatest.sh              run the sequence, print each boot
#   ./tools/otatest.sh --serial F   also concatenate the six logs into F
set -uo pipefail

JUNGEY_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=preflight.sh
. "$JUNGEY_ROOT/tools/preflight.sh"
preflight yes
cd "$JUNGEY_ROOT"

SERIAL=""
for ((i = 1; i <= $#; i++)); do
    case "${!i}" in
        --serial) j=$((i + 1)); SERIAL="${!j}" ;;
        -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    esac
done

( cd kernel && cargo build --release ) || exit 1
OBJCOPY="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/llvm-objcopy"
"$OBJCOPY" -O binary \
    kernel/target/aarch64-unknown-none-softfloat/release/jkernel \
    kernel/target/jkernel.bin || exit 1

# Its own disk, started empty: the sequence is driven by the control block
# this run writes, so it must not inherit one.
DISK="ota.img"
rm -f "$DISK"
truncate -s 64M "$DISK"

RUN="$(mktemp -d)"
trap 'rm -rf "$RUN"' EXIT

# Up to ten, not exactly six: the filesystem's own crash-consistency test cuts
# the power on two of the early boots, before the update machinery is reached,
# so those cycles pass without advancing the sequence. Stopping on the verdict
# rather than on a boot count means this does not have to know how many.
LAST=0
for boot in $(seq 1 10); do
    timeout 60 qemu-system-aarch64 \
        -M virt,gic-version=3 -cpu cortex-a72 -smp 4 -m 2G \
        -global virtio-mmio.force-legacy=false \
        -drive file="$DISK",if=none,format=raw,id=hd0 \
        -device virtio-blk-device,drive=hd0 \
        -kernel kernel/target/jkernel.bin \
        -display none -serial "file:$RUN/boot$boot.txt" </dev/null >/dev/null 2>&1
    LAST=$boot
    if grep -q "  update     :" "$RUN/boot$boot.txt" 2>/dev/null; then
        echo "--- power cycle $boot ---"
        sed -n '/^  update     :/,/^  heap check after stage 7c/p' "$RUN/boot$boot.txt" \
            | sed '/heap check/d; /^$/d'
    else
        echo "--- power cycle $boot (power cut during the filesystem test) ---"
    fi
    grep -q "RESULT     : PASS — a version that never came up" "$RUN/boot$boot.txt" && break
done

if [ -n "$SERIAL" ]; then
    : >"$SERIAL"
    for boot in $(seq 1 "$LAST"); do
        echo "=== power cycle $boot ===" >>"$SERIAL"
        cat "$RUN/boot$boot.txt" >>"$SERIAL" 2>/dev/null || true
    done
fi

grep -q "RESULT     : PASS — a version that never came up" "$RUN/boot$LAST.txt" 2>/dev/null
