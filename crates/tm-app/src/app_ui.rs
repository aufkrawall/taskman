//! UI chrome wrapper that adds the Windows Explorer restart command while
//! keeping the existing chrome implementation intact.

// `tab_header` is intentionally overridden below; keep the original chrome
// implementation intact without warning about that shadowed function.
#[allow(dead_code)]
mod original;
pub use original::*;

use eframe::egui;
use tm_core::i18n::{self, K};

use crate::app::{ProcessIdentity, TaskManApp};
use crate::icons::Icon;
use crate::theme::Palette;

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
    let snapshot = app.latest_snapshot()?;
    let process = snapshot.process(target.pid)?;
    (!process.synthetic
        && process.start_epoch_s == target.start_epoch_s
        && process.name.eq_ignore_ascii_case("explorer.exe"))
    .then_some(target)
}

#[cfg(target_os = "windows")]
fn restart_explorer(app: &mut TaskManApp, ctx: &egui::Context, target: ProcessIdentity) {
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
                && original::cmd_button(
                    ui,
                    pal,
                    Icon::Restart,
                    i18n::tr(K::RestartService),
                    true,
                )
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
    ui.add_space(2.0);
}
