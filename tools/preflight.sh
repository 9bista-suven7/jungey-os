# Sourced by run.sh, sim.sh and test.sh. Checks the handful of things this
# repository needs and says exactly how to get them, rather than letting the
# shell report "cargo: command not found" and leaving you to work it out.

preflight() {
    local want_qemu="${1:-yes}"
    local missing=0

    if ! command -v cargo >/dev/null 2>&1; then
        # A common case: rustup is installed but this shell never sourced its
        # environment. Fix it silently rather than sending someone to install
        # what they already have.
        if [ -f "$HOME/.cargo/env" ]; then
            # shellcheck disable=SC1091
            . "$HOME/.cargo/env"
        fi
    fi

    if ! command -v cargo >/dev/null 2>&1; then
        cat >&2 <<'MSG'
error: cargo not found — this builds a bare-metal AArch64 kernel and needs Rust.

  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  . "$HOME/.cargo/env"

Use rustup rather than your distribution's rust package: the kernel needs
Rust 1.82 or newer, and distribution packages are usually older.
MSG
        missing=1
    else
        if ! rustup target list --installed 2>/dev/null | grep -q aarch64-unknown-none-softfloat; then
            cat >&2 <<'MSG'
error: the bare-metal AArch64 target is not installed.

  rustup target add aarch64-unknown-none-softfloat
  rustup component add llvm-tools
MSG
            missing=1
        fi
        local sysroot host objcopy
        sysroot="$(rustc --print sysroot)"
        host="$(rustc -vV | sed -n 's/^host: //p')"
        objcopy="$sysroot/lib/rustlib/$host/bin/llvm-objcopy"
        if [ ! -x "$objcopy" ]; then
            cat >&2 <<'MSG'
error: llvm-objcopy not found — it flattens the kernel ELF into a bootable image.

  rustup component add llvm-tools
MSG
            missing=1
        fi
    fi

    if [ "$want_qemu" = yes ] && ! command -v qemu-system-aarch64 >/dev/null 2>&1; then
        cat >&2 <<'MSG'
error: qemu-system-aarch64 not found — it is the machine this OS runs on.

  Debian/Ubuntu:  sudo apt install qemu-system-arm
  Fedora:         sudo dnf install qemu-system-aarch64
  macOS:          brew install qemu

QEMU 6.0 or newer; sim.sh also needs virtio-gpu, which 6.0 has.
MSG
        missing=1
    fi

    [ "$missing" = 0 ] || exit 1
}
