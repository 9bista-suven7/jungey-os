use std::path::PathBuf;
use std::process::Command;

// The same SHA-256 the kernel uses, compiled into the build script. Measured
// boot where the recorder and the checker implement the hash separately is a
// measured boot that can disagree with itself for reasons that have nothing to
// do with the image.
#[allow(dead_code)] // the kernel uses more of it than the build script does
mod sha256 {
    include!("src/sha256.rs");
}

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    build_userspace(&dir);
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=src/boot.s");
    println!("cargo:rerun-if-changed=src/sha256.rs");
    println!("cargo:rerun-if-changed=src/vectors.s");
}

/// Build `os/user` and hand the kernel the resulting ELF to embed.
///
/// Nested cargo, deliberately: the init image is part of the kernel's own
/// build, so `cargo build` in os/kernel always produces a bootable pair. The
/// child gets its own target directory and a cleaned environment, or it
/// inherits this build's flags and fights over the same lock.
fn build_userspace(kernel_dir: &str) {
    let user_dir = PathBuf::from(kernel_dir).join("../user");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("user-target");

    println!("cargo:rerun-if-changed={}/src", user_dir.display());
    println!("cargo:rerun-if-changed={}/linker.ld", user_dir.display());
    println!("cargo:rerun-if-changed={}/Cargo.toml", user_dir.display());

    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(&user_dir)
        .arg("build")
        .arg("--release")
        .env("CARGO_TARGET_DIR", &out_dir);
    for k in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "CARGO_BUILD_TARGET"] {
        cmd.env_remove(k);
    }

    let status = cmd.status().expect("failed to run cargo for os/user");
    assert!(status.success(), "userspace build failed");

    let elf = out_dir.join("aarch64-unknown-none-softfloat/release/init");
    assert!(elf.exists(), "userspace ELF missing at {}", elf.display());
    println!("cargo:rustc-env=JUNGEY_INIT_ELF={}", elf.display());

    // Record what the kernel should expect to find inside itself. This is a
    // measurement, not a signature: it binds the init image to the kernel that
    // embeds it, and it is only as trustworthy as the kernel image. Making it
    // more than that needs a key the hardware protects.
    let bytes = std::fs::read(&elf).expect("read the userspace ELF");
    let mut hex = sha256::hex(&sha256::digest(&bytes));

    // `run.sh --tamper` builds a kernel whose recorded digest is wrong, so the
    // refusal can be watched end to end rather than asserted. Corrupting the
    // expectation is the same shape of failure as corrupting the image and is
    // very much easier to arrange from a shell script.
    println!("cargo:rerun-if-env-changed=JUNGEY_TAMPER");
    if std::env::var("JUNGEY_TAMPER").is_ok() {
        hex[63] = if hex[63] == b'0' { b'1' } else { b'0' };
    }
    println!(
        "cargo:rustc-env=JUNGEY_INIT_SHA256={}",
        std::str::from_utf8(&hex).unwrap()
    );
}
