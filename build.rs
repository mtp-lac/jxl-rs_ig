fn main() {
    // Cargo will automatically rebuild if any source file or Cargo.toml changes.
    println!("cargo:rerun-if-changed=src/");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rustc-check-cfg=cfg(jxl_try_as_icc_target)");
    if let Ok(manifest) = std::fs::read_to_string("Cargo.toml") {
        if manifest.contains("libjxl/jxl-rs") {
            println!("cargo:rustc-cfg=jxl_try_as_icc_target");
        }
    }
}
