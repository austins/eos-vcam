//! Build script for eosvcam-shared: generates the Win32 bindings listed in windows-bindings.txt
//! into `$OUT_DIR/windows_bindings.rs` (included by src/windows_bindings.rs).

fn main() {
    println!("cargo:rerun-if-changed=windows-bindings.txt");
    let out = std::path::Path::new(&std::env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"))
        .join("windows_bindings.rs");
    windows_bindgen::builder()
        .output(out)
        .filter_file("windows-bindings.txt")
        .flat()
        .write();
}
