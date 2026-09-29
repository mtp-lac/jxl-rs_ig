fn main() {
    // Cargo will automatically rebuild if any source file changes
    println!("cargo:rerun-if-changed=src/");
}