fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    // Keep PT_LOAD segments 4 KiB aligned; lld defaults to 64 KiB on aarch64,
    // which would make the kernel's loader round up eight times as much memory.
    println!("cargo:rustc-link-arg=-zmax-page-size=4096");
    println!("cargo:rerun-if-changed=linker.ld");
}
