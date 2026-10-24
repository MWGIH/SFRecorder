//! Generates the V4L2 bindings from the kernel headers and compiles the
//! compute shader to SPIR-V, so neither is written by hand.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed=src/v4l2_wrapper.h");
    bindgen::Builder::default()
        .header("src/v4l2_wrapper.h")
        .allowlist_type("v4l2_.*")
        .allowlist_var("V4L2_.*")
        .derive_default(true)
        .generate()
        .expect("generating V4L2 bindings")
        .write_to_file(out.join("v4l2.rs"))
        .expect("writing V4L2 bindings");

    println!("cargo:rerun-if-changed=shaders/convert.comp");
    let spv = out.join("convert.spv");
    let status = Command::new("glslangValidator")
        .args(["-V", "--target-env", "vulkan1.1", "-o"])
        .arg(&spv)
        .arg("shaders/convert.comp")
        .status()
        .expect("running glslangValidator (install glslang)");
    assert!(status.success(), "shader failed to compile");
}
