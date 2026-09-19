#!/usr/bin/env bash
# Run Jungey OS as a simulated device, with a screen.
#
# QEMU is already a full hardware simulator — run.sh uses it for the headless
# machine. This adds the parts that make it a *device*: a display the OS drives
# through its own userspace GPU driver, and a way to capture what is on it.
#
#   ./sim.sh                 boot with a screen and save screenshots
#   ./sim.sh --gui           open a window instead (needs a desktop)
#   ./sim.sh --shots N       how many frames to capture (default 3)
#   ./sim.sh --out DIR       where to put them (default ./screenshots)
set -euo pipefail

cd "$(dirname "$0")"

# Fail with something useful if the toolchain is missing.
# shellcheck source=tools/preflight.sh
. "$(dirname "$0")/tools/preflight.sh"
preflight yes

GUI=0
SHOTS=3
OUT="screenshots"
for ((i = 1; i <= $#; i++)); do
    case "${!i}" in
        --gui) GUI=1 ;;
        --shots) j=$((i + 1)); SHOTS="${!j}" ;;
        --out)   j=$((i + 1)); OUT="${!j}" ;;
        -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    esac
done

( cd kernel && cargo build --release )
OBJCOPY="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/llvm-objcopy"
"$OBJCOPY" -O binary \
    kernel/target/aarch64-unknown-none-softfloat/release/jkernel \
    kernel/target/jkernel.bin

# Its own disk, so a demo run does not disturb the state test.sh sequences.
DISK="sim.img"
[ -f "$DISK" ] || truncate -s 64M "$DISK"

QEMU=(
    qemu-system-aarch64
    -M virt,gic-version=3 -cpu cortex-a72 -smp 4 -m 2G
    -global virtio-mmio.force-legacy=false
    -drive file="$DISK",if=none,format=raw,id=hd0
    -device virtio-blk-device,drive=hd0
    -device virtio-gpu-device,xres=480,yres=960
    -kernel kernel/target/jkernel.bin
)

if [ "$GUI" = 1 ]; then
    exec "${QEMU[@]}" -serial stdio
fi

# Headless: drive QEMU over QMP, capture frames, convert to PNG.
mkdir -p "$OUT"
RUN="$(mktemp -d)"
trap 'rm -rf "$RUN"' EXIT

"${QEMU[@]}" -display none \
    -serial "file:$RUN/serial.txt" \
    -qmp "unix:$RUN/qmp.sock,server,nowait" &
QEMU_PID=$!

python3 - "$RUN" "$OUT" "$SHOTS" <<'PY'
import json, os, socket, struct, sys, time, zlib

run, out, shots = sys.argv[1], sys.argv[2], int(sys.argv[3])

def connect(path, tries=60):
    for _ in range(tries):
        try:
            s = socket.socket(socket.AF_UNIX); s.connect(path); return s
        except (FileNotFoundError, ConnectionRefusedError):
            time.sleep(0.1)
    raise SystemExit("qemu did not open its monitor")

s = connect(run + "/qmp.sock")
f = s.makefile("rwb")
f.readline()

def cmd(c, **a):
    """Send a QMP command. Returns None once QEMU has gone, rather than raising:
    the guest powering off mid-capture is an ordinary outcome, not a crash."""
    try:
        f.write((json.dumps({"execute": c, "arguments": a} if a else {"execute": c}) + "\n").encode())
        f.flush()
        while True:
            line = f.readline()
            if not line:
                return None
            m = json.loads(line)
            if "return" in m or "error" in m:
                return m
    except (BrokenPipeError, ConnectionResetError, ValueError):
        return None

cmd("qmp_capabilities")

def ppm_to_png(src, dst):
    """The screendump is a PPM; PNG is what anyone can actually open."""
    data = open(src, "rb").read()
    if not data.startswith(b"P6"):
        return False
    # header: P6 <ws> width <ws> height <ws> maxval <single ws> pixels
    fields, i = [], 2
    while len(fields) < 3:
        while i < len(data) and data[i : i + 1].isspace():
            i += 1
        if data[i : i + 1] == b"#":
            while data[i : i + 1] not in (b"\n", b""):
                i += 1
            continue
        j = i
        while j < len(data) and not data[j : j + 1].isspace():
            j += 1
        fields.append(int(data[i:j])); i = j
    i += 1
    w, h, _ = fields
    px = data[i : i + w * h * 3]

    raw = b"".join(b"\x00" + px[y * w * 3 : (y + 1) * w * 3] for y in range(h))
    def chunk(tag, body):
        return (struct.pack(">I", len(body)) + tag + body
                + struct.pack(">I", zlib.crc32(tag + body) & 0xFFFFFFFF))
    png = (b"\x89PNG\r\n\x1a\n"
           + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
           + chunk(b"IDAT", zlib.compress(raw, 9))
           + chunk(b"IEND", b""))
    open(dst, "wb").write(png)
    return True

# The screen comes up after the boot demos, so wait for the driver to say so.
deadline = time.time() + 90
serial = run + "/serial.txt"
while time.time() < deadline:
    try:
        if "attached in userspace: 480x960" in open(serial, errors="replace").read():
            break
    except FileNotFoundError:
        pass
    time.sleep(0.2)

saved = []
for n in range(shots):
    time.sleep(0.6)
    ppm = f"{run}/shot{n}.ppm"
    if cmd("screendump", filename=ppm) is None:
        break
    time.sleep(0.4)
    dst = f"{out}/jungey-{n + 1}.png"
    if os.path.exists(ppm) and ppm_to_png(ppm, dst):
        saved.append(dst)

for p in saved:
    print("captured", p, os.path.getsize(p), "bytes")
if not saved:
    print("no frames captured")

try:
    cmd("quit")
except Exception:
    pass
PY

wait "$QEMU_PID" 2>/dev/null || true
echo
echo "serial log:"
sed -n '/display    :/,$p' "$RUN/serial.txt" | head -8 || true
