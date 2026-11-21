//! UI chrome wrapper that adds the Windows Explorer restart command while
//! keeping the existing chrome implementation intact.

mod original;
pub use original::*;

use eframe::egui;
use tm_core::i18n::{self, K};

#[cfg(target_os = "windows")]
use crate::app::ProcessIdentity;
use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::theme::Palette;

/// Gate shared by the toolbar button and both tab context menus: `identity`
/// belongs to a live, non-synthetic `explorer.exe` in the latest snapshot.
/// The image name is read from the snapshot, never from the clicked row's
/// label — the Processes page shows display names like
/// "Windows Explorer (explorer.exe)". The start-time comparison keeps a
/// recycled PID from being mistaken for the shell. `row_synthetic` is only a
/// cheap pre-check for pseudo-rows ("System interrupts"), which have no OS
/// process behind them.
#[cfg(target_os = "windows")]
pub(crate) fn explorer_restart_identity(
    app: &TaskManApp,
    identity: ProcessIdentity,
    row_synthetic: bool,
) -> Option<ProcessIdentity> {
    if row_synthetic || !app.actions.capabilities().end_process {
        return None;
    }
    let shell_owner_pid = tm_platform::win::shell_window_owner_pid()?;
    let snapshot = app.latest_snapshot()?;
    let process = snapshot.process(identity.pid)?;
    is_shell_explorer_process(process, &identity, Some(shell_owner_pid)).then_some(identity)
}

/// Pure core of [`explorer_restart_identity`]: the snapshot entry behind
/// `identity` is a non-synthetic `explorer.exe` of the same process
/// generation with a verified creation timestamp. Matching uses the image
/// name (`process.name`), never a display label — the Processes page renders
/// "Windows Explorer (explorer.exe)", so a row label must not be what decides
/// this.
#[cfg(target_os = "windows")]
fn is_live_explorer_process(
    process: &tm_core::model::ProcessEntry,
    identity: &ProcessIdentity,
) -> bool {
    !process.synthetic
        && identity.start_epoch_s.is_some_and(|start| start > 0)
        && process.start_epoch_s == identity.start_epoch_s
        && process.name.eq_ignore_ascii_case("explorer.exe")
}

#[cfg(target_os = "windows")]
fn is_shell_explorer_process(
    process: &tm_core::model::ProcessEntry,
    identity: &ProcessIdentity,
    shell_owner_pid: Option<u32>,
) -> bool {
    shell_owner_pid == Some(identity.pid) && is_live_explorer_process(process, identity)
}

#[cfg(target_os = "windows")]
fn explorer_restart_target(app: &TaskManApp) -> Option<ProcessIdentity> {
    if !matches!(
        app.tab,
        crate::app::Tab::Processes | crate::app::Tab::Details
    ) || app.selection.len() != 1
    {
        return None;
    }

    let target = app.selection.primary()?.clone();
    // Selection only ever holds real process rows, never pseudo-rows.
    explorer_restart_identity(app, target, false)
}

#[cfg(target_os = "windows")]
pub(crate) fn restart_explorer(app: &mut TaskManApp, ctx: &egui::Context, target: ProcessIdentity) {
    if !app.actions.capabilities().end_process {
        return;
    }
    if !app.identity_is_live(&target) {
        app.shared.toast(i18n::tr(K::ProcessExited));
        return;
    }

    let pid = target.pid;
    let start = target.start_epoch_s;
    app.run_action_refreshing(
        ctx,
        || i18n::tr(K::RestartService).to_string(),
        move || tm_platform::win::restart_explorer(pid, start),
    );
}

/// Per-tab command header. On Windows, a single live `explorer.exe` selection
/// on either Processes or Details gets the same Restart command native Task
/// Manager exposes for the shell process.
pub fn tab_header(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    pal: &Palette,
    extra: impl FnOnce(&mut TaskManApp, &mut egui::Ui),
    menu: impl FnOnce(&mut TaskManApp, &mut egui::Ui),
) {
    let title = app.tab.label();
    let selected = matches!(
        app.tab,
        crate::app::Tab::Processes | crate::app::Tab::Details
    )
    .then(|| app.selection.len())
    .filter(|count| *count > 1);
    #[cfg(target_os = "windows")]
    let restart_target = explorer_restart_target(app);

    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.label(egui::RichText::new(title).size(15.5).strong());
        if let Some(count) = selected {
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(i18n::trf(K::SelectedCount, &[&count.to_string()]))
                    .size(12.5)
                    .color(pal.text_dim),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            original::ellipsis_menu(app, ui, pal, menu);
            extra(app, ui);
            #[cfg(target_os = "windows")]
            if let Some(target) = restart_target
                && original::cmd_button(ui, pal, Icon::Restart, i18n::tr(K::RestartService), true)
            {
                let ctx = ui.ctx().clone();
                restart_explorer(app, &ctx, target);
            }
            original::vsep(ui, pal);
            #[cfg(target_os = "windows")]
            {
                if original::cmd_button(
                    ui,
                    pal,
                    Icon::OpenExternal,
                    i18n::tr(K::WindowsTaskManager),
                    true,
                ) {
                    let actions = app.actions.clone();
                    let ctx = ui.ctx().clone();
                    app.run_action(
                        &ctx,
                        || i18n::tr(K::WindowsTaskManagerStarted).to_string(),
                        move || actions.launch_native_task_manager(),
                    );
                }
                original::vsep(ui, pal);
            }
            if original::cmd_button(ui, pal, Icon::RunTask, i18n::tr(K::RunNewTask), true) {
                app.run_dialog_open = true;
            }
        });
        ui.add_space(4.0);
    });
    if ui.is_enabled() {
        original::finish_toolbar_focus(ui.ctx(), app.tab);
    }
    ui.add_space(2.0);
}

/// Submenu for switching a table's row spacing between `Normal` and `Compact`.
/// Returns `true` when the setting changed so the caller can persist it.
pub fn row_density_submenu(ui: &mut egui::Ui, density: &mut tm_core::settings::RowDensity) -> bool {
    use tm_core::settings::RowDensity;
    let mut changed = false;
    crate::widgets::menu::submenu(ui, i18n::tr(K::RowSpacing), |ui| {
        if crate::widgets::menu::check(
            ui,
            i18n::tr(K::RowDensityNormal),
            *density == RowDensity::Normal,
        )
        .clicked()
        {
            if *density != RowDensity::Normal {
                *density = RowDensity::Normal;
                changed = true;
            }
            ui.close();
        }
        if crate::widgets::menu::check(
            ui,
            i18n::tr(K::RowDensityCompact),
            *density == RowDensity::Compact,
        )
        .clicked()
        {
            if *density != RowDensity::Compact {
                *density = RowDensity::Compact;
                changed = true;
            }
            ui.close();
        }
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::settings::RowDensity;

    #[test]
    fn row_height_maps_density_variants() {
        assert_eq!(
            crate::widgets::tablekit::row_height(RowDensity::Normal),
            crate::widgets::tablekit::ROW_H
        );
        assert_eq!(
            crate::widgets::tablekit::row_height(RowDensity::Compact),
            crate::widgets::tablekit::ROW_H_DENSE
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn explorer_gate_matches_image_name_not_display_label() {
        use tm_core::model::ProcessEntry;

        let mut explorer = ProcessEntry::new(4242, "EXPLORER.EXE");
        explorer.display = "Windows Explorer".to_string();
        explorer.start_epoch_s = Some(1_700_000_000);
        let identity = ProcessIdentity {
            pid: 4242,
            start_epoch_s: Some(1_700_000_000),
        };
        assert!(is_live_explorer_process(&explorer, &identity));

        // Same display label but a different image name must not qualify.
        let mut impostor = ProcessEntry::new(4243, "not_explorer.exe");
        impostor.display = "Windows Explorer".to_string();
        impostor.start_epoch_s = Some(1_700_000_000);
        let identity = ProcessIdentity {
            pid: 4243,
            start_epoch_s: Some(1_700_000_000),
        };
        assert!(!is_live_explorer_process(&impostor, &identity));

        // Recycled PID: the start time must match the clicked generation.
        let identity = ProcessIdentity {
            pid: 4242,
            start_epoch_s: Some(1_700_000_001),
        };
        assert!(!is_live_explorer_process(&explorer, &identity));

        // Synthetic pseudo-rows never qualify.
        let mut synth = ProcessEntry::new(4244, "explorer.exe");
        synth.synthetic = true;
        let identity = ProcessIdentity {
            pid: 4244,
            start_epoch_s: None,
        };
        assert!(!is_live_explorer_process(&synth, &identity));
    }

    #[test]
    fn row_density_submenu_renders_without_panicking() {
        let ctx = egui::Context::default();
        let mut density = RowDensity::Compact;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let changed = row_density_submenu(ui, &mut density);
            assert!(!changed);
        });
        out.textures_delta.clear();
        assert_eq!(density, RowDensity::Compact);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn shell_explorer_gate_requires_timestamp_and_matching_shell_owner() {
        use tm_core::model::ProcessEntry;

        let mut entry = ProcessEntry::new(400, "explorer.exe");
        entry.start_epoch_s = Some(1_700_000_000);
        let id = ProcessIdentity {
            pid: 400,
            start_epoch_s: Some(1_700_000_000),
        };
        assert!(is_shell_explorer_process(&entry, &id, Some(400)));
        assert!(
            !is_shell_explorer_process(&entry, &id, Some(999)),
            "another session's explorer.exe must be rejected"
        );
        assert!(
            !is_shell_explorer_process(&entry, &id, None),
            "missing shell window owner must be rejected"
        );

        let no_ts = ProcessIdentity {
            pid: 400,
            start_epoch_s: None,
        };
        entry.start_epoch_s = None;
        assert!(
            !is_shell_explorer_process(&entry, &no_ts, Some(400)),
            "missing creation identity must fail closed"
        );
    }
}
