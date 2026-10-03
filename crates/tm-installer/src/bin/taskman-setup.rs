//! taskman-setup - the Task Manager setup wizard and silent installer.
//!
//! Release builds hide the console (`windows` subsystem); CLI output
//! (`--help`, `--dry-run`) re-attaches to the parent console when present.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use tm_installer::options::{self, Mode, Options};

fn main() {
    // Setup runs elevated from a user-writable folder. Before anything can
    // trigger a delay-loaded import, confine DLL loading to System32; then
    // pin the running image so later reads cannot be redirected by renaming
    // it (see `tm_installer::win`).
    #[cfg(windows)]
    {
        if let Err(error) = tm_installer::win::restrict_dll_search_to_system32() {
            attach_console();
            eprintln!("taskman-setup: {error}");
            std::process::exit(1);
        }
        tm_installer::win::pin_setup_image();
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match options::parse(args) {
        Ok(opts) => opts,
        Err(error) => {
            attach_console();
            eprintln!("taskman-setup: {error}");
            eprintln!();
            eprintln!("{}", options::USAGE);
            std::process::exit(2);
        }
    };

    if opts.help {
        attach_console();
        println!("{}", options::USAGE);
        return;
    }
    if opts.dry_run {
        attach_console();
        print_plan(&opts);
        return;
    }
    std::process::exit(run(opts));
}

fn attach_console() {
    #[cfg(windows)]
    tm_installer::win::attach_parent_console();
}

fn print_plan(opts: &Options) {
    let verb = match opts.mode {
        Mode::Install => "install",
        Mode::Uninstall => "uninstall",
    };
    println!("planned {verb} steps:");
    for line in tm_installer::install::describe(opts) {
        println!("  {line}");
    }
    if let Some(dir) = tm_installer::install::install_dir_for_display() {
        println!("install location: {}", dir.display());
    }
}

fn run(opts: Options) -> i32 {
    #[cfg(not(windows))]
    {
        let _ = opts;
        eprintln!("taskman-setup: the installer is Windows-only");
        return 1;
    }
    #[cfg(windows)]
    {
        if !opts.silent {
            return match tm_installer::ui::run(opts) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("taskman-setup: {error}");
                    1
                }
            };
        }

        // Silent mode: same plan, no window, exit code 0/1.
        attach_console();
        let mut emit = |event: tm_installer::install::Event| {
            if let tm_installer::install::Event::Step(index, state, detail) = &event {
                let line = format!("step {}: {state:?} {detail}", index + 1);
                println!("{line}");
                tm_installer::win::append_setup_log(&line);
            }
        };
        let result = match opts.mode {
            Mode::Install => tm_installer::install::install(&opts, &mut emit),
            Mode::Uninstall => tm_installer::install::uninstall(&opts, &mut emit),
        };
        match result {
            Ok(()) => {
                tm_installer::win::append_setup_log("setup finished successfully");
                0
            }
            Err(error) => {
                let line = format!("setup failed: {error}");
                eprintln!("taskman-setup: {line}");
                tm_installer::win::append_setup_log(&line);
                1
            }
        }
    }
}
