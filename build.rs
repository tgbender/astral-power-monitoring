use std::{env, path::PathBuf, process::Command};
fn main() {
    println!("cargo:rerun-if-changed=resources/app.rc");
    println!("cargo:rerun-if-changed=assets/icons/monitor.ico");
    println!("cargo:rerun-if-changed=assets/icons/warning.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("app.res");
    let status = Command::new("rc.exe")
        .arg("/nologo")
        .arg("/fo")
        .arg(&output)
        .arg("resources/app.rc")
        .status()
        .expect("Windows SDK resource compiler rc.exe is required on PATH");
    assert!(status.success(), "Icon resource compilation failed");
    println!("cargo:rustc-link-arg={}", output.display());
}
