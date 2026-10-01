//! tm-installer - the self-contained Windows setup for Task Man.
//!
//! `taskman-setup.exe` is a single file: the wizard plus an embedded,
//! hash-verified payload archive (see [`payload`]). It installs to the
//! protected `%ProgramFiles%\TaskMan` location and registers the background
//! service through the app's own elevated `--core-service` helper, so the
//! hardened service lifecycle keeps a single owner. The wizard renders with
//! `tm-ui`, the task manager's theme module, so the installer mirrors the
//! app's Windows 11 look in dark and light mode.
//!
//! `taskman-payload` is the build-time tool `build.py` uses to append the
//! payload archive to the linked setup executable.

pub mod install;
pub mod options;
pub mod payload;
#[cfg(windows)]
mod staging;
#[cfg(windows)]
pub mod ui;
#[cfg(windows)]
mod user;
#[cfg(windows)]
pub mod win;
