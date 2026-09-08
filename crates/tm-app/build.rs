fn main() {
    // Build scripts are compiled for the HOST, so `cfg!(target_os = "windows")`
    // here would describe the build machine, not the artifact. A Windows host
    // cross-building for Linux would then pass the Windows resource file to the
    // Linux linker (`app.res: unknown file type`). Cargo exposes the real
    // target OS as CARGO_CFG_TARGET_OS; use that instead.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let res = std::path::Path::new("assets/app.res");
        if res.exists() {
            println!(
                "cargo:rustc-link-arg={}",
                res.canonicalize().unwrap().display()
            );
        }
    }
}
