//! Install and uninstall orchestration.
//!
//! The work is expressed as an ordered list of steps so the wizard and the
//! silent mode report exactly the same progress, and so `--dry-run` can show
//! the real plan instead of a paraphrase of it.
//!
//! What this module deliberately does NOT do: reimplement the protected
//! service lifecycle. Files + SCM registration run through the app's own
//! elevated `--core-service=install` helper, which owns the pinned-copy,
//! ACL, broker-manifest and SCM logic (`tm-platform::win::core_service`).
//! The installer only supplies the payload and the user-facing extras
//! (shortcuts, Add/Remove Programs entry, launch).

use std::path::PathBuf;

use tm_core::error::{Result, TmError};

use crate::options::Options;
#[cfg(windows)]
use crate::payload::Archive;

#[cfg(windows)]
use crate::win;

/// Payload entry names that are not binaries but still belong in the install
/// directory (the service helper only manages the two executables).
#[cfg(windows)]
const EXTRA_ENTRIES: &[&str] = &["LICENSE"];

pub const APP_EXE: &str = "taskman.exe";
pub const SERVICE_EXE: &str = "taskman-service.exe";
pub const SETUP_EXE: &str = "taskman-setup.exe";
pub const SHORTCUT_NAME: &str = "Task Manager.lnk";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Done,
    Skipped,
    Failed,
}

/// One progress notification. `Step` reports (index, state, detail) where the
/// index refers to the step list returned by [`install_steps`] /
/// [`uninstall_steps`].
#[derive(Debug, Clone)]
pub enum Event {
    Step(usize, StepState, String),
    /// Free-form progress detail (also written to the setup log).
    Log(String),
    /// Terminal notification carrying the overall result.
    Finished(std::result::Result<(), String>),
}

type Sink<'a> = &'a mut dyn FnMut(Event);

#[cfg(windows)]
fn fail(context: &'static str, detail: impl Into<String>) -> TmError {
    TmError::platform(context, detail)
}

/// The steps `install` will run for these options, in order.
pub fn install_steps(opts: &Options) -> Vec<&'static str> {
    let mut steps = vec![
        // The payload is extracted and SHA-256 verified FIRST, before
        // anything on the machine is touched: a bare build output (no
        // embedded payload) or a corrupt artifact must fail here, not after
        // the running app has been stopped.
        "Extract installer payload",
        "Stop running Task Manager",
        // With the service on, files and SCM registration are one elevated
        // helper transaction; splitting it into two "steps" would report one
        // of them twice.
        if opts.service {
            "Install program files and register service"
        } else {
            "Install program files"
        },
    ];
    if opts.start_menu || opts.desktop {
        steps.push("Create shortcuts");
    }
    steps.push("Register uninstaller in Settings");
    if opts.launch {
        steps.push("Launch Task Manager");
    }
    steps
}

/// The steps `uninstall` will run, in order.
pub fn uninstall_steps() -> Vec<&'static str> {
    vec![
        "Stop running Task Manager",
        "Remove background service",
        "Remove shortcuts",
        "Remove uninstaller from Settings",
        "Remove program files",
    ]
}

/// Human-readable plan for `--dry-run`.
pub fn describe(opts: &Options) -> Vec<String> {
    let steps = match opts.mode {
        crate::options::Mode::Install => install_steps(opts),
        crate::options::Mode::Uninstall => uninstall_steps(),
    };
    steps
        .into_iter()
        .enumerate()
        .map(|(index, label)| format!("{}. {label}", index + 1))
        .collect()
}

/// Install (or upgrade) Task Manager. Safe to run over an existing install:
/// the service helper stops the old service generation before replacing files.
pub fn install(opts: &Options, emit: Sink<'_>) -> Result<()> {
    #[cfg(not(windows))]
    {
        let _ = (opts, emit);
        Err(TmError::Unsupported("taskman-setup"))
    }
    #[cfg(windows)]
    {
        if !win::is_elevated() {
            return Err(fail(
                "install",
                "administrator rights are required to install Task Manager",
            ));
        }
        let sid = win::current_user_sid()?;
        let install_dir = win::install_dir()?;
        let setup_exe = std::env::current_exe()?;
        let staging =
            std::env::temp_dir().join(format!("taskman-setup-payload-{}", std::process::id()));

        let mut at = 0usize;
        let mut step = |index: usize, state: StepState, detail: String| {
            emit(Event::Step(index, state, detail));
        };

        // 1. The payload is extracted to a private temp directory and every
        //    entry is SHA-256 verified BEFORE anything on the machine is
        //    touched. A bare build output (no embedded payload) or a corrupt
        //    artifact must fail here - not after the running app has been
        //    stopped.
        step(at, StepState::Running, "verifying embedded payload".into());
        let archive = Archive::open(&setup_exe)?;
        archive.entry(APP_EXE)?;
        archive.entry(SERVICE_EXE)?;
        let extracted = archive.extract_to(&staging)?;
        step(
            at,
            StepState::Done,
            format!("{} files verified", extracted.len()),
        );
        at += 1;

        // 2. A running GUI holds its own image open; close it before files
        //    move under it. Portable copies elsewhere are left alone - they
        //    are not part of this install.
        step(
            at,
            StepState::Running,
            "checking for running instances".into(),
        );
        let stopped = win::stop_running_gui(&install_dir.join(APP_EXE))?;
        step(
            at,
            StepState::Done,
            if stopped == 0 {
                "no running instance".into()
            } else {
                format!("stopped {stopped} running instance(s)")
            },
        );
        at += 1;

        // 3. Program files. With the service on this is the app's own
        //    elevated helper transaction (files + ACLs + broker manifest +
        //    SCM registration + service start).
        step(at, StepState::Running, install_dir.display().to_string());
        if opts.service {
            win::run_core_service_helper(&staging.join(APP_EXE), "install", &sid)?;
            step(
                at,
                StepState::Done,
                "service TaskmanCore registered and started".into(),
            );
        } else {
            std::fs::create_dir_all(&install_dir)?;
            for name in [APP_EXE, SERVICE_EXE] {
                std::fs::copy(staging.join(name), install_dir.join(name))?;
            }
            step(at, StepState::Done, install_dir.display().to_string());
        }
        // LICENSE and friends are outside the helper's scope.
        for name in EXTRA_ENTRIES {
            let source = staging.join(name);
            if source.is_file() {
                let _ = std::fs::copy(&source, install_dir.join(name));
            }
        }
        at += 1;

        // 4. Shortcuts (per-user: the setup runs as the installing admin).
        if opts.start_menu || opts.desktop {
            step(at, StepState::Running, "writing shortcuts".into());
            let gui = install_dir.join(APP_EXE);
            if opts.start_menu {
                let lnk = win::start_menu_dir()?.join(SHORTCUT_NAME);
                win::create_shortcut(&lnk, &gui, &install_dir, &gui, "Task Manager")?;
            }
            if opts.desktop {
                let lnk = win::desktop_dir()?.join(SHORTCUT_NAME);
                win::create_shortcut(&lnk, &gui, &install_dir, &gui, "Task Manager")?;
            }
            step(at, StepState::Done, "shortcuts created".into());
            at += 1;
        }

        // 5. Add/Remove Programs entry. Its UninstallString points at the
        //    setup copy we place in the install directory.
        step(
            at,
            StepState::Running,
            "writing uninstall registry key".into(),
        );
        std::fs::copy(&setup_exe, install_dir.join(SETUP_EXE))?;
        win::write_arp(
            &install_dir,
            env!("CARGO_PKG_VERSION"),
            win::dir_size_kb(&install_dir),
        )?;
        step(at, StepState::Done, "registered in Settings > Apps".into());
        at += 1;

        // 6. Optional launch.
        if opts.launch {
            step(at, StepState::Running, "starting Task Manager".into());
            win::launch(&install_dir.join(APP_EXE))?;
            step(at, StepState::Done, "started".into());
        }

        let _ = std::fs::remove_dir_all(&staging);
        Ok(())
    }
}

/// Uninstall Task Manager: service, shortcuts, ARP entry and program files.
///
/// Per-user settings are preserved (they live in the user profile, not in the
/// install tree).
pub fn uninstall(opts: &Options, emit: Sink<'_>) -> Result<()> {
    #[cfg(not(windows))]
    {
        let _ = (opts, emit);
        Err(TmError::Unsupported("taskman-setup"))
    }
    #[cfg(windows)]
    {
        let _ = opts;
        if !win::is_elevated() {
            return Err(fail(
                "uninstall",
                "administrator rights are required to uninstall Task Manager",
            ));
        }
        let sid = win::current_user_sid()?;
        let install_dir = win::install_dir()?;

        let mut at = 0usize;
        let mut step = |index: usize, state: StepState, detail: String| {
            emit(Event::Step(index, state, detail));
        };

        step(
            at,
            StepState::Running,
            "checking for running instances".into(),
        );
        let stopped = win::stop_running_gui(&install_dir.join(APP_EXE))?;
        step(
            at,
            StepState::Done,
            if stopped == 0 {
                "no running instance".into()
            } else {
                format!("stopped {stopped} running instance(s)")
            },
        );
        at += 1;

        // The installed helper owns service removal; it intentionally leaves
        // binaries behind, which is exactly the division of labor we want.
        step(
            at,
            StepState::Running,
            "removing service TaskmanCore".into(),
        );
        let installed_gui = install_dir.join(APP_EXE);
        if installed_gui.is_file() {
            win::run_core_service_helper(&installed_gui, "uninstall", &sid)?;
            step(at, StepState::Done, "service removed".into());
        } else {
            win::delete_service_fallback()?;
            step(
                at,
                StepState::Done,
                "installed Task Manager missing; service removed directly".into(),
            );
        }
        at += 1;

        step(at, StepState::Running, "removing shortcuts".into());
        let mut removed = 0usize;
        for dir in [win::start_menu_dir(), win::desktop_dir()]
            .into_iter()
            .flatten()
        {
            let lnk = dir.join(SHORTCUT_NAME);
            if std::fs::remove_file(&lnk).is_ok() {
                removed += 1;
            }
        }
        step(
            at,
            StepState::Done,
            format!("removed {removed} shortcut(s)"),
        );
        at += 1;

        step(
            at,
            StepState::Running,
            "removing uninstall registry key".into(),
        );
        win::remove_arp()?;
        step(at, StepState::Done, "removed".into());
        at += 1;

        // Files last. The running setup exe may live INSIDE the tree (the ARP
        // UninstallString points there), so it is renamed out first - a
        // running image can be renamed but not deleted - and scheduled for
        // deletion at the next reboot afterwards.
        step(at, StepState::Running, "removing program files".into());
        let orphan = win::relocate_self_out_of(&install_dir)?;
        let mut detail = Vec::new();
        if install_dir.is_dir() {
            std::fs::remove_dir_all(&install_dir).map_err(|error| {
                fail(
                    "remove program files",
                    format!("{}: {error}", install_dir.display()),
                )
            })?;
            detail.push("program files removed".to_string());
        } else {
            detail.push("program files were already gone".to_string());
        }
        // Logs + broker manifest: best-effort, never fails the uninstall.
        match win::service_data_dir() {
            Ok(data_dir) if data_dir.is_dir() => match std::fs::remove_dir_all(&data_dir) {
                Ok(()) => detail.push("service data removed".to_string()),
                Err(error) => detail.push(format!("service data kept: {error}")),
            },
            _ => {}
        }
        if let Some(orphan) = orphan {
            let _ = win::schedule_delete_at_reboot(&orphan);
            detail.push("setup cleanup scheduled".to_string());
        }
        step(at, StepState::Done, detail.join("; "));

        Ok(())
    }
}

/// Absolute paths the install/uninstall touches; used by `--dry-run` and the
/// wizard's confirmation text.
pub fn install_dir_for_display() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        win::install_dir().ok()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Whether an existing install is present (drives the wizard's Upgrade /
/// Uninstall affordances).
pub fn existing_install() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let dir = win::install_dir().ok()?;
        dir.join(APP_EXE).is_file().then_some(dir)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::Mode;

    #[test]
    fn install_steps_follow_the_chosen_options() {
        let mut opts = Options {
            mode: Mode::Install,
            ..Options::default()
        };
        let steps = install_steps(&opts);
        assert_eq!(
            steps,
            [
                "Extract installer payload",
                "Stop running Task Manager",
                "Install program files and register service",
                "Create shortcuts",
                "Register uninstaller in Settings",
            ]
        );

        opts.service = false;
        opts.start_menu = false;
        opts.launch = true;
        let steps = install_steps(&opts);
        assert_eq!(
            steps,
            [
                "Extract installer payload",
                "Stop running Task Manager",
                "Install program files",
                "Register uninstaller in Settings",
                "Launch Task Manager",
            ],
            "no service step and no shortcut step when disabled"
        );
    }

    /// The payload must be proven good before ANYTHING on the machine is
    /// disturbed: a bare build output or corrupt artifact may not stop the
    /// user's running app first (that ordering cost a real debugging detour).
    #[test]
    fn payload_verification_precedes_touching_the_machine() {
        let steps = install_steps(&Options::default());
        assert_eq!(
            steps[0], "Extract installer payload",
            "payload check must be the first step: {steps:?}"
        );
        assert!(
            steps.iter().position(|s| *s == "Stop running Task Manager")
                > steps.iter().position(|s| *s == "Extract installer payload"),
            "stopping the app may not come before the payload check: {steps:?}"
        );
    }

    /// The default options must install the service: first GUI start has to
    /// reach the broker without a UAC prompt.
    #[test]
    fn default_options_install_the_service() {
        let steps = install_steps(&Options::default());
        assert!(
            steps.contains(&"Install program files and register service"),
            "service install is part of the default plan: {steps:?}"
        );
    }

    #[test]
    fn describe_numbers_the_real_steps() {
        let lines = describe(&Options::default());
        assert_eq!(lines.len(), install_steps(&Options::default()).len());
        assert!(lines[0].starts_with("1. "));
    }
}
