fn main() {
    // Re-embed (and thus rebuild) when the static frontend changes.
    println!("cargo:rerun-if-changed=frontend");
    tauri_build::build();
}
