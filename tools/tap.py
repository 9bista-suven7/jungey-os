#!/usr/bin/env python3
"""Drive the guest's pointer over QMP: wait until the OS says it is ready,
then tap four places and let it report.

The taps are chosen so that the third one lands on the *same pixel* as the
first and has to go to a different application, because the one underneath
was raised in between. That is the check that a compositor is doing
something rather than remembering who asked last.
"""
import json, socket, sys, time

RUN = sys.argv[1]
WIDTH, HEIGHT = 480, 960

# (x, y, what it should reach) — the comment is the expectation, the OS
# decides for itself.
TAPS = [
    (240, 370, "notes, which is on top here"),
    (240, 190, "the shell, which is the only window there — and it raises"),
    (240, 370, "the shell now, at the very same pixel"),
    (240, 700, "nothing at all"),
]


def connect(path, tries=120):
    for _ in range(tries):
        try:
            s = socket.socket(socket.AF_UNIX)
            s.connect(path)
            return s
        except (FileNotFoundError, ConnectionRefusedError):
            time.sleep(0.1)
    raise SystemExit("qemu did not open its monitor")


sock = connect(RUN + "/qmp.sock")
f = sock.makefile("rwb")
f.readline()


def cmd(c, **a):
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


def axis(px, extent):
    """Screen pixel to the 0..32767 range an absolute device reports."""
    return ((px * 2 + 1) * 32767) // (2 * extent)


def tap(x, y):
    cmd(
        "input-send-event",
        events=[
            {"type": "abs", "data": {"axis": "x", "value": axis(x, WIDTH)}},
            {"type": "abs", "data": {"axis": "y", "value": axis(y, HEIGHT)}},
            {"type": "btn", "data": {"button": "left", "down": True}},
        ],
    )
    time.sleep(0.15)
    cmd("input-send-event", events=[{"type": "btn", "data": {"button": "left", "down": False}}])


def serial():
    try:
        return open(RUN + "/serial.txt", errors="replace").read()
    except FileNotFoundError:
        return ""


def wait_for(text, seconds):
    end = time.time() + seconds
    while time.time() < end:
        if text in serial():
            return True
        time.sleep(0.2)
    return False


if not wait_for("ui         : ready for input", 90):
    print("the guest never reported a pointer; is the tablet attached?")
    cmd("quit")
    raise SystemExit(2)

for x, y, expectation in TAPS:
    print(f"  tap {x},{y} -> expecting {expectation}")
    tap(x, y)
    time.sleep(0.5)

# Let the OS finish its own report, then let it power itself off.
wait_for("RESULT     : PASS — every tap reached", 30) or wait_for("RESULT     :", 5)
wait_for("stage 6 complete", 30)
cmd("quit")
