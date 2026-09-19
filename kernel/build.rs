use std::path::PathBuf;
use std::process::Command;

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    build_userspace(&dir);
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=src/boot.s");
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
}
