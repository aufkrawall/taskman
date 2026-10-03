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
        // DLL planting: setup runs elevated from a user-writable folder
        // (Downloads), and the loader resolves static imports that are not
        // KnownDLLs from the executable's own folder before System32. Those
        // imports are delay-loaded instead, so nothing is resolved before
        // `main` restricts the search path to System32
        // (`win::restrict_dll_search_to_system32`). A listed DLL that a given
        // feature set does not import only produces a linker note (LNK4199).
        // `tests/import_table.rs` fails when a new non-KnownDLL import
        // appears, so this list cannot silently go stale.
        for dll in DELAY_LOADED {
            println!("cargo:rustc-link-arg-bin=taskman-setup=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-arg-bin=taskman-setup=delayimp.lib");
        // VCRUNTIME140.dll cannot be delay-loaded (the CRT startup runs on
        // it), so this binary alone uses the hybrid CRT: VC runtime and
        // startup code linked statically, the Universal CRT still dynamic
        // through `ucrtbase.dll`, which is itself a KnownDLL. rustc requests
        // the dynamic CRT as `/defaultlib:msvcrt`, so the swap is per-binary;
        // the rest of the workspace keeps the default CRT.
        for arg in [
            "/NODEFAULTLIB:msvcrt.lib",
            "/NODEFAULTLIB:vcruntime.lib",
            "/NODEFAULTLIB:libucrt.lib",
            "/DEFAULTLIB:libcmt.lib",
            "/DEFAULTLIB:libvcruntime.lib",
            "/DEFAULTLIB:ucrt.lib",
        ] {
            println!("cargo:rustc-link-arg-bin=taskman-setup={arg}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}

/// Non-KnownDLL imports of `taskman-setup` (across the per-package and the
/// whole-workspace feature sets), plus `bcryptprimitives.dll`, which is only
/// a KnownDLL transitively on current Windows builds rather than by registry
/// entry.
const DELAY_LOADED: &[&str] = &[
    "bcryptprimitives.dll",
    "dwmapi.dll",
    "dwrite.dll",
    "dxgi.dll",
    "opengl32.dll",
    "uiautomationcore.dll",
    "userenv.dll",
    "uxtheme.dll",
];
