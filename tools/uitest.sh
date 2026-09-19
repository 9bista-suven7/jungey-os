#!/usr/bin/env bash
# Stage 4a exit test: boot with a screen and a pointer, and tap it.
#
# QEMU is the only thing here that is not the OS. It emulates the display and
# the tablet; the taps are injected over QMP, the same way a human moving a
# finger would reach the guest, and everything past the virtio-mmio registers
# is the system under test.
#
#   ./tools/uitest.sh              boot, tap, print the verdict
#   ./tools/uitest.sh --serial F   also write the whole serial log to F
#   ./tools/uitest.sh --gui        watch it happen in a window
set -uo pipefail

JUNGEY_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=preflight.sh
. "$JUNGEY_ROOT/tools/preflight.sh"
preflight yes
cd "$JUNGEY_ROOT"

GUI=0
SERIAL=""
for ((i = 1; i <= $#; i++)); do
    case "${!i}" in
        --gui) GUI=1 ;;
        --serial) j=$((i + 1)); SERIAL="${!j}" ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    esac
done

( cd kernel && cargo build --release ) || exit 1
OBJCOPY="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/llvm-objcopy"
"$OBJCOPY" -O binary \
    kernel/target/aarch64-unknown-none-softfloat/release/jkernel \
    kernel/target/jkernel.bin || exit 1

# Its own disk: this boot writes files and must not disturb the crash test's
# carefully sequenced image.
DISK="ui.img"
rm -f "$DISK"
truncate -s 64M "$DISK"

RUN="$(mktemp -d)"
trap 'rm -rf "$RUN"' EXIT

QEMU=(
    qemu-system-aarch64
    -M virt,gic-version=3 -cpu cortex-a72 -smp 4 -m 2G
    -global virtio-mmio.force-legacy=false
    -drive file="$DISK",if=none,format=raw,id=hd0
    -device virtio-blk-device,drive=hd0
    -device virtio-gpu-device,xres=480,yres=960
    -device virtio-tablet-device
    -kernel kernel/target/jkernel.bin
)

if [ "$GUI" = 1 ]; then
    exec "${QEMU[@]}" -serial stdio
fi

"${QEMU[@]}" -display none \
    -serial "file:$RUN/serial.txt" \
    -qmp "unix:$RUN/qmp.sock,server,nowait" &
QEMU_PID=$!

python3 "$JUNGEY_ROOT/tools/tap.py" "$RUN"
rc=$?

wait "$QEMU_PID" 2>/dev/null || true
[ -n "$SERIAL" ] && cp "$RUN/serial.txt" "$SERIAL"

echo
sed -n '/  ui         : shell is pid/,$p' "$RUN/serial.txt" | head -40
exit "$rc"
