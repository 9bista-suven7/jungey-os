#!/usr/bin/env bash
# Run the full test suite: every stage's exit test, end to end, from an empty
# disk. Prints one line per check and exits non-zero if any of them fail.
#
#   ./test.sh            six boots: SMP, capabilities, block I/O, crash test,
#                        userspace driver, shared model pages, tensor scheduling,
#                        the agent runtime, and a screen you can tap
#   ./test.sh --repeat N  run the whole thing N times over, for flushing out
#                         races that only show up occasionally
set -uo pipefail

# Fail with something useful if the toolchain is missing, rather than letting
# the shell report "cargo: command not found". Resolved before any cd, so it
# works whatever directory you invoke this from.
JUNGEY_ROOT="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tools/preflight.sh
. "$JUNGEY_ROOT/tools/preflight.sh"
preflight yes

cd "$(dirname "$0")"

LOG_DIR="$(mktemp -d)"
trap 'rm -rf "$LOG_DIR"' EXIT

REPEAT=1
[ "${1:-}" = "--repeat" ] && REPEAT="${2:-1}"

fail=0
check() { # check <description> <expected count> <log> <pattern>
    local n
    n=$(grep -c -- "$4" "$3" 2>/dev/null || true)
    if [ "$n" = "$2" ]; then
        printf '  \033[32mok\033[0m   %s\n' "$1"
    else
        printf '  \033[31mFAIL\033[0m %s (matched %s, wanted %s)\n' "$1" "$n" "$2"
        fail=$((fail + 1))
    fi
}

for round in $(seq 1 "$REPEAT"); do
    [ "$REPEAT" -gt 1 ] && echo "=== round $round of $REPEAT ==="

    # The crash test spans five boots and sequences itself through a marker on
    # the disk, so the disk must persist across them and start empty.
    # Every boot is bounded: a kernel that hangs should fail the suite, not
    # wedge it. A healthy boot takes about a second.
    timeout 60 ./run.sh --fresh >"$LOG_DIR/boot1" 2>&1 </dev/null
    for b in 2 3 4 5; do
        timeout 60 ./run.sh >"$LOG_DIR/boot$b" 2>&1 </dev/null
    done

    echo "boot 1 — first light on an empty disk"
    check "all four cores online"        1 "$LOG_DIR/boot1" "smp        : 4 of 4 cores online"
    check "capability message delivered" 1 "$LOG_DIR/boot1" 'got: "hello from the sender"'
    check "rights are attenuated"        1 "$LOG_DIR/boot1" "refused, as it should be: EPERM"
    check "no ambient authority"         1 "$LOG_DIR/boot1" "denied: EBADCAP"
    check "revocation kills the subtree" 2 "$LOG_DIR/boot1" "dead: EREVOKED"
    check "cross-core lock loses nothing" 1 "$LOG_DIR/boot1" "lock           : PASS"
    check "block write reads back"       1 "$LOG_DIR/boot1" "read back  : PASS"
    check "filesystem formatted"         1 "$LOG_DIR/boot1" "format     : superblock"
    check "driver runs in userspace"     1 "$LOG_DIR/boot1" "attached in userspace"
    check "driver holds five capabilities" 1 "$LOG_DIR/boot1" "holding 5 capabilities"
    check "interactive met its deadline" 1 "$LOG_DIR/boot1" "deadline MET"
    check "background gave way"          1 "$LOG_DIR/boot1" "the background job gave way"
    check "undeliverable work refused"   1 "$LOG_DIR/boot1" "refused, as it should be — the app"
    check "opportunistic waited"         1 "$LOG_DIR/boot1" "opportunistic work waited for an idle"
    check "scheduler result"             1 "$LOG_DIR/boot1" "RESULT     : PASS — interactive work preempted"
    check "kv spilled under pressure"    1 "$LOG_DIR/boot1" "blocks spilled to flash"
    check "thermal refuses background"   1 "$LOG_DIR/boot1" "background work        REFUSED"
    check "interactive still admitted"   1 "$LOG_DIR/boot1" "RESULT     : PASS — past the throttle point"
    check "energy cap binds"             1 "$LOG_DIR/boot1" "energy cap : 40 segments"
    check "agent composed operations"    1 "$LOG_DIR/boot1" "published operations it was not"
    check "delegation chain readable"    1 "$LOG_DIR/boot1" "journal-writer"
    check "action log intact"            1 "$LOG_DIR/boot1" "chain      : intact"
    check "task undone byte for byte"    1 "$LOG_DIR/boot1" "byte-for-byte what they were before: true"
    check "tampering detected"           1 "$LOG_DIR/boot1" "chain breaks at"
    check "agent runtime result"         1 "$LOG_DIR/boot1" "RESULT     : PASS — the assistant composed"
    check "image measured before it runs" 1 "$LOG_DIR/boot1" "bytes  sha256 .*matches the build"
    check "flipped byte is refused"      1 "$LOG_DIR/boot1" "rejected — it is not the image"
    check "measured boot result"         1 "$LOG_DIR/boot1" "RESULT     : PASS — the userspace image is measured"
    check "idle machine stops asking"    1 "$LOG_DIR/boot1" "fewer wakeups for the same"
    check "the clock survived the quiet" 1 "$LOG_DIR/boot1" "the clock is the counter, not the tick"
    check "power result"                 1 "$LOG_DIR/boot1" "RESULT     : PASS — an idle machine takes"
    check "kv survived the flash trip"   1 "$LOG_DIR/boot1" "every byte survived the round trip"
    check "kv policy correct"            1 "$LOG_DIR/boot1" "RESULT     : PASS — the budget held"
    check "model file verified on disk"  1 "$LOG_DIR/boot1" "verified on disk"
    check "weights shared, not copied"   1 "$LOG_DIR/boot1" "one copy of the weights"
    check "reclaimed pages re-faulted"   2 "$LOG_DIR/boot1" "0 wrong after"
    check "no kernel panic"              0 "$LOG_DIR/boot1" "KERNEL PANIC"
    check "no wait timed out"            0 "$LOG_DIR/boot1" "TIMEOUT"
    check "boot ran to completion"       1 "$LOG_DIR/boot1" "boot sequence complete"

    echo "boot 2 — verify v1, then lose power part way through the data"
    check "sector survived the reboot"   1 "$LOG_DIR/boot2" "previous   : boot 1"
    check "sector body intact"           1 "$LOG_DIR/boot2" "body verified"
    check "v1 readable"                  1 "$LOG_DIR/boot2" "PASS — hello.txt holds v1"
    check "power cut mid-data"           1 "$LOG_DIR/boot2" "power cut after 1 of"

    echo "boot 3 — recovered; now lose power with the data complete"
    check "filesystem still mounts"      1 "$LOG_DIR/boot3" "mounted    : checkpoint seq"
    check "mid-data crash left no trace" 1 "$LOG_DIR/boot3" "the mid-data crash left no trace"
    check "v1 still intact"              1 "$LOG_DIR/boot3" "PASS — hello.txt holds v1"
    check "power cut before commit"      1 "$LOG_DIR/boot3" "checkpoint skipped"

    echo "boot 4 — the hardest case: data complete, commit missing"
    check "filesystem still mounts"      1 "$LOG_DIR/boot4" "mounted    : checkpoint seq"
    check "uncommitted file is invisible" 1 "$LOG_DIR/boot4" "uncommitted file is invisible"
    check "v1 still intact"              1 "$LOG_DIR/boot4" "PASS — hello.txt holds v1"
    check "v2 committed cleanly"         1 "$LOG_DIR/boot4" "v2 committed"

    echo "boot 5 — the clean write survived"
    check "v2 readable"                  1 "$LOG_DIR/boot5" "PASS — hello.txt holds v2"
    check "crash test complete"          1 "$LOG_DIR/boot5" "crash consistency test complete"
    check "driver served every request"  1 "$LOG_DIR/boot5" "requests served by the userspace driver"
    check "weights shared on remount"    1 "$LOG_DIR/boot5" "one copy of the weights"
    check "model survived the crashes"   1 "$LOG_DIR/boot5" "verified on disk"
    check "no kernel panic"              0 "$LOG_DIR/boot5" "KERNEL PANIC"
    check "no wait timed out"            0 "$LOG_DIR/boot5" "TIMEOUT"
    check "scheduler still correct"      1 "$LOG_DIR/boot5" "RESULT     : PASS — interactive work preempted"
    check "boot ran to completion"       1 "$LOG_DIR/boot5" "boot sequence complete"

    echo "boot 6 — a screen, a pointer, and two applications"
    # A separate boot, on its own disk: it needs a display and an input device,
    # and the crash test above deliberately cuts the power, which is
    # incompatible with having anything to look at.
    timeout 180 ./tools/uitest.sh --serial "$LOG_DIR/ui" >"$LOG_DIR/uirun" 2>&1 </dev/null
    check "pointer driven from userspace"  1 "$LOG_DIR/ui" "\[inputdrv\] attached in userspace"
    check "input driver holds four caps"   1 "$LOG_DIR/ui" "inputdrv is pid .*holding 4 capabilities"
    check "both applications opened"       1 "$LOG_DIR/ui" "2 windows open"
    check "tap reached the top window"     1 "$LOG_DIR/ui" "tap at 239,370 -> pid 12 window 1"
    check "tap reached the other window"   1 "$LOG_DIR/ui" "tap at 239,190 -> pid 11 window 0"
    check "same point, raised window"      1 "$LOG_DIR/ui" "tap at 239,370 -> pid 11 window 0"
    check "tap outside every window"       1 "$LOG_DIR/ui" "hit no window"
    check "applications saw their taps"    1 "$LOG_DIR/ui" 'row 0 "GROCERIES" is now on'
    check "raised window got the tap"      1 "$LOG_DIR/ui" 'row 5 "SLEEP" is now on'
    check "window ownership enforced"      1 "$LOG_DIR/ui" "1 operation(s) named a window the sender does not own"
    check "only the damage is redrawn"     1 "$LOG_DIR/ui" "smallest transfer"
    check "ui result"                      1 "$LOG_DIR/ui" "RESULT     : PASS — every tap reached exactly one window"
    check "no kernel panic"                0 "$LOG_DIR/ui" "KERNEL PANIC"
    check "boot ran to completion"         1 "$LOG_DIR/ui" "boot sequence complete"

    echo "boot 7 — a kernel that expects a different image"
    # Built with the wrong digest recorded, so the refusal can be watched end
    # to end rather than asserted. It powers off before the block driver
    # starts, so it cannot disturb the disk the boots above sequenced.
    timeout 60 ./run.sh --tamper >"$LOG_DIR/tamper" 2>&1 </dev/null
    check "mismatch is reported"           1 "$LOG_DIR/tamper" "MISMATCH"
    check "userspace is refused"           1 "$LOG_DIR/tamper" "refusing to start userspace"
    check "nothing ran at EL0"             0 "$LOG_DIR/tamper" "wrote its pid to"

    echo "boots 8+ — an update that never comes up, and the one after it"
    timeout 600 ./tools/otatest.sh --serial "$LOG_DIR/ota" >"$LOG_DIR/otarun" 2>&1 </dev/null
    check "fresh device initialised"       1 "$LOG_DIR/ota" "this device has never been updated"
    check "staged into the spare slot"     1 "$LOG_DIR/ota" "staged     version 2 into slot B"
    check "slot measured before use"       3 "$LOG_DIR/ota" "digest matches the control block"
    check "a try is spent per boot"        2 "$LOG_DIR/ota" "self-test  FAILED"
    check "rolled back unattended"         1 "$LOG_DIR/ota" "ran out of tries — rolled back to slot A"
    check "the next update was kept"       1 "$LOG_DIR/ota" "self-test  passed — marked successful"
    check "update result"                  1 "$LOG_DIR/ota" "RESULT     : PASS — a version that never came up"

done

echo
if [ "$fail" = 0 ]; then
    printf '\033[32mall checks passed\033[0m\n'
else
    printf '\033[31m%d check(s) failed\033[0m\n' "$fail"
fi
exit $((fail > 0))
