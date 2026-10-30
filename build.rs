//! The icon, into the executable.
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=data");
    let object = format!("{}/app.o", std::env::var("OUT_DIR").unwrap());
    // From Linux, the cross compiler's; on Windows, MinGW's own.
    let done = ["x86_64-w64-mingw32-windres", "windres"].iter()
        .any(|tool| Command::new(tool).args(["data/app.rc", "-O", "coff", "-o", &object]).status().is_ok_and(|s| s.success()));
    assert!(done, "windres failed");
    println!("cargo:rustc-link-arg={object}");
}
