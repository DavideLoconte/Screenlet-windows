//! The icon, into the executable.
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=data");
    let object = format!("{}/app.o", std::env::var("OUT_DIR").unwrap());
    let done = Command::new("x86_64-w64-mingw32-windres").args(["data/app.rc", "-O", "coff", "-o", &object]).status();
    assert!(done.is_ok_and(|s| s.success()), "windres failed");
    println!("cargo:rustc-link-arg={object}");
}
