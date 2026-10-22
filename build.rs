fn main() {
    // Only embed Windows resources when targeting Windows.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // Minimal .rc: exe icon + requireAdministrator manifest.
    // (Numeric resource types so windres needs no windows.h include path.)
    std::fs::write(
        out.join("uc.rc"),
        "101 ICON \"assets/UtilityCloset.ico\"\n1 24 \"assets/UtilityCloset.manifest\"\n",
    )
    .unwrap();

    // Compile the .rc with windres (x86_64-w64-mingw32-windres on PATH as `windres`).
    let status = std::process::Command::new("windres")
        .args([
            "--input-format=rc",
            "--output-format=coff",
            "--target=pe-x86-64",
        ])
        .arg(out.join("uc.rc"))
        .arg(out.join("resource.o"))
        .status()
        .expect("failed to run windres (is mingw-w64 installed?)");
    assert!(status.success(), "windres failed");

    // Link the object directly (not via an archive, which the linker can discard).
    println!("cargo:rustc-link-arg={}", out.join("resource.o").display());
    println!("cargo:rerun-if-changed=assets/UtilityCloset.ico");
    println!("cargo:rerun-if-changed=assets/UtilityCloset.manifest");
}
