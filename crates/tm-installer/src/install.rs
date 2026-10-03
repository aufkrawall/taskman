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
            "Install program files without background service"
        },
        // Right after the service: once a LocalSystem service exists, the
        // install must be removable even if a later, cosmetic step fails.
        "Register uninstaller in Settings",
    ];
    if opts.start_menu || opts.desktop {
        steps.push("Create shortcuts");
    }
    if opts.launch {
        steps.push("Launch Task Manager");
    }
    steps
}

/// The steps `uninstall` will run, in order.
pub fn uninstall_steps() -> Vec<&'static str> {
    vec![
        "Restore Windows Task Manager",
        "Stop running Task Manager",
        "Remove background service",
        "Remove shortcuts",
        "Remove program files",
        "Remove uninstaller from Settings",
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

#[cfg(any(windows, test))]
fn install_program_files(
    service: bool,
    mut helper: impl FnMut(&str) -> Result<()>,
    copy_files: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if service {
        helper("install")
    } else {
        helper("uninstall")?;
        copy_files()
    }
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
        let user = crate::user::InstallUser::resolve()?;
        let sid = user.sid()?;
        if opts.launch {
            user.ensure_launchable()?;
        }
        let install_dir = win::install_dir()?;
        // The payload and the uninstaller copy come from the image pinned at
        // startup, never from re-opening the launch path.
        let setup_image = win::setup_image()?;

        let mut at = 0usize;
        let mut step = |index: usize, state: StepState, detail: String| {
            emit(Event::Step(index, state, detail));
        };

        // 1. The payload is extracted to protected staging and every
        //    entry is SHA-256 verified BEFORE anything on the machine is
        //    touched. A bare build output (no embedded payload) or a corrupt
        //    artifact must fail here - not after the running app has been
        //    stopped.
        step(at, StepState::Running, "verifying embedded payload".into());
        let archive = Archive::parse(setup_image.read_all()?)?;
        archive.entry(APP_EXE)?;
        archive.entry(SERVICE_EXE)?;
        archive.verify_all()?;
        let staging_guard = crate::staging::Staging::create()?;
        let staging = staging_guard.path();
        let extracted = archive.extract_to(staging)?;
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
        // Before the service (re)starts and checks its log directory: a
        // setup.log left there by older setups disables its file logging.
        let legacy_log = match win::remove_legacy_setup_log() {
            Ok(true) => "; removed legacy setup log from the service log directory".to_string(),
            Ok(false) => String::new(),
            Err(error) => format!("; legacy setup log kept: {error}"),
        };
        install_program_files(
            opts.service,
            |operation| win::run_core_service_helper(&staging.join(APP_EXE), operation, &sid),
            || {
                tm_platform::win::core_service::install_program_files_without_service(
                    &staging.join(APP_EXE),
                )
            },
        )?;
        if opts.service {
            step(
                at,
                StepState::Done,
                format!("service TaskmanCore registered and started{legacy_log}"),
            );
        } else {
            step(
                at,
                StepState::Done,
                format!("{}{legacy_log}", install_dir.display()),
            );
        }
        // LICENSE and friends are outside the helper's scope.
        for name in EXTRA_ENTRIES {
            let source = staging.join(name);
            if source.is_file() {
                let _ = std::fs::copy(&source, install_dir.join(name));
            }
        }
        at += 1;

        // 4. Add/Remove Programs entry, immediately after the service step:
        //    from here on a failing later step still leaves an install that
        //    Settings > Apps can remove. Its UninstallString points at the
        //    setup copy placed in the install directory; when setup already
        //    runs from there, that copy is the running image itself.
        step(
            at,
            StepState::Running,
            "writing uninstall registry key".into(),
        );
        let copied = setup_image.copy_to(&install_dir.join(SETUP_EXE))?;
        win::write_arp(
            &install_dir,
            env!("CARGO_PKG_VERSION"),
            win::dir_size_kb(&install_dir),
        )?;
        step(
            at,
            StepState::Done,
            if copied {
                "registered in Settings > Apps".into()
            } else {
                "registered in Settings > Apps (uninstaller already in place)".into()
            },
        );
        at += 1;

        // 5. Shortcuts belong to the desktop user, not the UAC administrator.
        if opts.start_menu || opts.desktop {
            step(at, StepState::Running, "writing shortcuts".into());
            let gui = install_dir.join(APP_EXE);
            if opts.start_menu {
                let lnk = user.start_menu_dir()?.join(SHORTCUT_NAME);
                win::create_shortcut(&lnk, &gui, &install_dir, &gui, "Task Manager")?;
            }
            if opts.desktop {
                let lnk = user.desktop_dir()?.join(SHORTCUT_NAME);
                win::create_shortcut(&lnk, &gui, &install_dir, &gui, "Task Manager")?;
            }
            step(at, StepState::Done, "shortcuts created".into());
            at += 1;
        }

        // 6. Optional launch.
        if opts.launch {
            step(at, StepState::Running, "starting Task Manager".into());
            user.launch(&install_dir.join(APP_EXE))?;
            step(at, StepState::Done, "started".into());
        }

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
        let user = crate::user::InstallUser::resolve()?;
        let sid = user.sid()?;
        let install_dir = win::install_dir()?;
        // Moving the uninstaller out of the tree (and back on failure) goes
        // through the pinned image; prove it is available before anything
        // is removed rather than halfway through.
        win::setup_image()?;

        let mut at = 0usize;
        let mut step = |index: usize, state: StepState, detail: String| {
            emit(Event::Step(index, state, detail));
        };

        step(
            at,
            StepState::Running,
            "checking Task Manager replacement".into(),
        );
        let restored = tm_platform::win::remove_task_manager_replacement_for_deleted_exe(
            &install_dir.join(APP_EXE),
        )?;
        step(
            at,
            StepState::Done,
            if restored {
                "restored Windows Task Manager"
            } else {
                "no matching replacement"
            }
            .into(),
        );
        at += 1;

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
        tm_platform::win::core_service::stop_existing_service_before_copy()?;
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
        for dir in [user.start_menu_dir(), user.desktop_dir()]
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

        // Files before ARP cleanup. Setup may live INSIDE the tree (the ARP
        // UninstallString points there), so it is renamed out first - a
        // running image can be renamed but not deleted - and scheduled for
        // deletion at the next reboot afterwards.
        step(at, StepState::Running, "removing program files".into());
        let orphan = win::relocate_self_out_of(&install_dir)?;
        let mut detail = Vec::new();
        if install_dir.is_dir() {
            if let Err(error) = std::fs::remove_dir_all(&install_dir) {
                // Keep the ARP command usable when removal fails halfway. The
                // move back goes through the pinned image handle, so only the
                // relocated uninstaller itself can return to the install tree.
                let rollback = orphan
                    .as_ref()
                    .map(|_| win::restore_setup_image(&install_dir.join(SETUP_EXE)));
                return Err(fail(
                    "remove program files",
                    match rollback {
                        Some(Err(rollback)) => {
                            format!("{error}; could not restore uninstaller: {rollback}")
                        }
                        _ => error.to_string(),
                    },
                ));
            }
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
            match win::schedule_delete_at_reboot(&orphan) {
                Ok(()) => detail.push("setup cleanup scheduled".to_string()),
                Err(error) => detail.push(format!("setup cleanup could not be scheduled: {error}")),
            }
        }
        step(at, StepState::Done, detail.join("; "));
        at += 1;
        step(
            at,
            StepState::Running,
            "removing uninstall registry key".into(),
        );
        win::remove_arp()?;
        step(at, StepState::Done, "removed".into());

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
    fn service_opt_out_stops_and_removes_the_old_broker_before_copying() {
        let order = std::cell::RefCell::new(Vec::new());
        install_program_files(
            false,
            |op| {
                order.borrow_mut().push(op.to_string());
                Ok(())
            },
            || {
                order.borrow_mut().push("copy".into());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*order.borrow(), ["uninstall", "copy"]);
        let copied = std::cell::Cell::new(false);
        assert!(
            install_program_files(
                false,
                |_| Err(TmError::Unsupported("stop failed")),
                || {
                    copied.set(true);
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(
            !copied.get(),
            "failed service removal must leave pinned binaries intact"
        );
    }

    #[test]
    fn service_install_keeps_binary_copy_owned_by_the_helper() {
        install_program_files(
            true,
            |op| {
                assert_eq!(op, "install");
                Ok(())
            },
            || panic!("never copy pinned service binaries outside the helper"),
        )
        .unwrap();
    }

    #[test]
    fn uninstall_restores_task_manager_before_deleting_files_and_removes_arp_last() {
        let steps = uninstall_steps();
        assert_eq!(steps[0], "Restore Windows Task Manager");
        assert_eq!(steps.last(), Some(&"Remove uninstaller from Settings"));
    }

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
                "Register uninstaller in Settings",
                "Create shortcuts",
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
                "Install program files without background service",
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

    /// Regression: shortcuts used to be created between the service step and
    /// the Add/Remove Programs registration, so a shortcut failure left a
    /// LocalSystem service behind with no uninstall entry. The uninstaller
    /// must be registered directly after the program files/service step,
    /// before any other step that can fail.
    #[test]
    fn uninstaller_is_registered_right_after_the_service_step() {
        for service in [true, false] {
            let opts = Options {
                service,
                desktop: true,
                launch: true,
                ..Options::default()
            };
            let steps = install_steps(&opts);
            let program_files = steps
                .iter()
                .position(|s| s.starts_with("Install program files"))
                .unwrap();
            assert_eq!(
                steps[program_files + 1],
                "Register uninstaller in Settings",
                "{steps:?}"
            );
            assert!(
                steps.iter().position(|s| *s == "Create shortcuts") > Some(program_files + 1),
                "{steps:?}"
            );
        }
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
