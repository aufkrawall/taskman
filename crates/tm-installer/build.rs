fn main() {
    // Build scripts are compiled for the HOST, so `cfg!(target_os = "windows")`
    // here would describe the build machine, not the artifact. A Windows host
    // cross-building for Linux would then pass the Windows resource file to the
    // Linux linker (`app.res: unknown file type`). Cargo exposes the real
    // target OS as CARGO_CFG_TARGET_OS; use that instead. (Same rationale as
    // tm-app's build.rs.)
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    if target_os == "windows" {
        // The setup carries the product icon so Explorer, the taskbar and the
        // title bar show the same icon as the app. The compiled resource is
        // shared with tm-app rather than duplicated.
        let res = std::path::Path::new("../tm-app/assets/app.res");
        if res.exists() {
            println!(
                "cargo:rustc-link-arg-bin=taskman-setup={}",
                res.canonicalize().unwrap().display()
            );
        }
    }

    // `taskman-setup` is an elevation boundary by design: it performs
    // machine-wide writes (Program Files, SCM service registration), so the
    // UAC prompt must appear before any window or file work happens. The
    // requireAdministrator manifest is attached ONLY to the setup target; the
    // `taskman-payload` build tool must keep running unelevated inside
    // `build.py`.
    if target_os == "windows" && target_env == "msvc" {
        println!("cargo:rustc-link-arg-bin=taskman-setup=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=taskman-setup=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'"
        );
    }
    println!("cargo:rerun-if-changed=build.rs");
}
