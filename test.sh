#!/usr/bin/env bash
# Run the full test suite: every stage's exit test, end to end, from an empty
# disk. Prints one line per check and exits non-zero if any of them fail.
#
#   ./test.sh            five boots: SMP, capabilities, block I/O, crash test,
#                        userspace driver, shared model pages, tensor scheduling
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
    check "kv survived the flash trip"   1 "$LOG_DIR/boot1" "every byte survived the round trip"
    check "kv policy correct"            1 "$LOG_DIR/boot1" "RESULT     : PASS — the budget held"
    check "model file verified on disk"  1 "$LOG_DIR/boot1" "verified on disk"
    check "weights shared, not copied"   1 "$LOG_DIR/boot1" "one copy of the weights"
    check "reclaimed pages re-faulted"   2 "$LOG_DIR/boot1" "0 wrong after"
    check "no kernel panic"              0 "$LOG_DIR/boot1" "KERNEL PANIC"
    check "no wait timed out"            0 "$LOG_DIR/boot1" "TIMEOUT"
    check "boot ran to completion"       1 "$LOG_DIR/boot1" "stage 5c complete"

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
    check "boot ran to completion"       1 "$LOG_DIR/boot5" "stage 5c complete"
done

echo
if [ "$fail" = 0 ]; then
    printf '\033[32mall checks passed\033[0m\n'
else
    printf '\033[31m%d check(s) failed\033[0m\n' "$fail"
fi
exit $((fail > 0))
