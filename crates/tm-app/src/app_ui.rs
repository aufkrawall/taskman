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

/// Width of `text` as the painter lays it out at `font`.
fn text_width(ui: &egui::Ui, text: &str, font: &egui::FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::WHITE)
        .size()
        .x
}

/// Shorten `text` so it renders no wider than `max_w` at `font`, marking the
/// cut with a single-character ellipsis. The page title gets a measured budget
/// rather than the whole row, so an overlong name has to be trimmed here
/// instead of being sliced mid-glyph by the row's clip rect.
fn elide_to_width(ui: &egui::Ui, text: &str, font: &egui::FontId, max_w: f32) -> String {
    if text_width(ui, text, font) <= max_w {
        return text.to_owned();
    }
    // Without room for the ellipsis itself there is nothing honest left to
    // show: a lone "…" would still paint past the budget.
    if max_w <= 0.0 || text_width(ui, "…", font) > max_w {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid]
            .iter()
            .copied()
            .chain(std::iter::once('…'))
            .collect();
        if text_width(ui, &candidate, font) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut out: String = chars[..lo].iter().collect();
    while out.ends_with(' ') {
        out.pop();
    }
    out.push('…');
    out
}

/// Split one header row between the page title and the command cluster.
///
/// The title owns the left edge and only the width `original::header_title_budget`
/// grants it; the cluster owns the right edge, inset by `HEADER_PAD_R` — the
/// same 8 px the row used to add as a leading space — and takes what is left.
/// Both rects come out of one split, so neither can grow into the other, which
/// is exactly what happened while the cluster was laid out right to left inside
/// a row it could overrun.
///
/// A row too narrow for padding, title and gap collapses the cluster onto the
/// right edge instead of producing a rect whose minimum sits past its maximum.
pub(crate) fn header_geometry(row: egui::Rect, title_full_w: f32) -> (egui::Rect, egui::Rect) {
    let title_left = row.left() + original::HEADER_PAD_L;
    let cluster_right = row.right() - original::HEADER_PAD_R;
    let title_budget = original::header_title_budget(row.width(), title_full_w);
    // The gap belongs to neither rect, so the two never touch even where the
    // row is too narrow to honor the budget.
    let title_right =
        (title_left + title_budget).min((cluster_right - original::HEADER_GAP).max(title_left));
    let title_rect = egui::Rect::from_min_max(
        egui::pos2(title_left, row.top()),
        egui::pos2(title_right, row.bottom()),
    );
    let cluster_left = title_right + original::HEADER_GAP;
    let cluster_rect = if cluster_left <= cluster_right {
        egui::Rect::from_min_max(
            egui::pos2(cluster_left, row.top()),
            egui::pos2(cluster_right, row.bottom()),
        )
    } else {
        egui::Rect::from_min_max(
            egui::pos2(cluster_right, row.top()),
            egui::pos2(cluster_right, row.bottom()),
        )
    };
    (title_rect, cluster_rect)
}

/// The command cluster's own ui: laid out right to left inside `rect` and
/// clipped to it.
///
/// A right-to-left layout walks leftwards from `rect`'s right edge and never
/// stops at its left edge, so a cluster wider than its share keeps going over
/// the title. The clip is the net that holds every command inside the cluster's
/// half of the row, and it keeps the clipped-away part unclickable too.
fn header_cluster_ui(ui: &mut egui::Ui, rect: egui::Rect) -> egui::Ui {
    let mut cluster = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    cluster.set_clip_rect(rect.intersect(ui.clip_rect()));
    cluster
}

/// Per-tab command header. On Windows, a single live `explorer.exe` selection
/// on either Processes or Details gets the same Restart command native Task
/// Manager exposes for the shell process.
///
/// The row is split once up front: the command cluster owns the right side and
/// the page title the left, each drawn into its own rect and clipped to it.
/// A cluster wider than its share now gives up the leftmost buttons instead of
/// painting them over the title.
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

    // One exact row, allocated before anything is drawn in it: the parent
    // cursor advances by the same height the old `ui.horizontal` produced, so
    // the table below keeps its position.
    let (row, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), original::HEADER_ROW_H),
        egui::Sense::hover(),
    );
    let title_font = crate::theme::font_semibold_in(ui, 15.5);
    let (title_rect, cluster_rect) = header_geometry(row, text_width(ui, title, &title_font));

    // The cluster is drawn first because the title is elided against the space
    // it leaves behind. Order and behavior are the ones the row always had.
    let mut cluster = header_cluster_ui(ui, cluster_rect);
    original::ellipsis_menu(app, &mut cluster, pal, menu);
    extra(app, &mut cluster);
    #[cfg(target_os = "windows")]
    if let Some(target) = restart_target
        && original::header_cmd_button(
            &mut cluster,
            pal,
            Icon::Restart,
            i18n::tr(K::RestartService),
            true,
        )
    {
        let ctx = cluster.ctx().clone();
        restart_explorer(app, &ctx, target);
    }
    original::vsep(&mut cluster, pal);
    #[cfg(target_os = "windows")]
    {
        if original::header_cmd_button(
            &mut cluster,
            pal,
            Icon::OpenExternal,
            i18n::tr(K::WindowsTaskManager),
            true,
        ) {
            let actions = app.actions.clone();
            let ctx = cluster.ctx().clone();
            app.run_action(
                &ctx,
                || i18n::tr(K::WindowsTaskManagerStarted).to_string(),
                move || actions.launch_native_task_manager(),
            );
        }
        original::vsep(&mut cluster, pal);
    }
    if original::header_cmd_button(
        &mut cluster,
        pal,
        Icon::RunTask,
        i18n::tr(K::RunNewTask),
        app.actions.capabilities().run_new_task,
    ) {
        app.run_dialog_open = true;
    }

    // Then the title strip, which ends where the cluster begins. The count is
    // measured as well, so a long page name can never push either of them under
    // the commands.
    let mut title_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(title_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    title_ui.set_clip_rect(title_rect.intersect(ui.clip_rect()));
    let elided = elide_to_width(&title_ui, title, &title_font, title_rect.width());
    let title_w = if elided.is_empty() {
        0.0
    } else {
        text_width(&title_ui, &elided, &title_font)
    };
    if !elided.is_empty() {
        title_ui
            .add(egui::Label::new(egui::RichText::new(elided.as_str()).font(title_font)).extend());
    }
    if let Some(count) = selected {
        let chip = i18n::trf(K::SelectedCount, &[&count.to_string()]);
        let chip_font = egui::FontId::proportional(12.0);
        let chip_w = text_width(&title_ui, &chip, &chip_font);
        let gap = title_ui.spacing().item_spacing.x;
        // The count is a hint next to the page name, so it yields when the row
        // is narrow; the page name never does.
        if title_rect.width() - title_w - gap >= 10.0 + chip_w {
            title_ui.add_space(10.0);
            title_ui.add(
                egui::Label::new(egui::RichText::new(chip).size(12.0).color(pal.text_dim)).extend(),
            );
        }
    }
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

    // ------------------------------------------------------ header row split

    fn header_row(width: f32) -> egui::Rect {
        egui::Rect::from_min_size(
            egui::pos2(0.0, 10.0),
            egui::vec2(width, original::HEADER_ROW_H),
        )
    }

    /// The reported defect: at 960 logical px the Processes cluster is wider
    /// than the space left of it, and a right-to-left layout walked straight
    /// over the page title. The split has to hold at every width the window can
    /// have, from the 720 px minimum upwards.
    #[test]
    fn title_and_command_cluster_never_share_a_pixel() {
        let mut width = 200.0;
        while width <= 2000.0 {
            let row = header_row(width);
            let (title, cluster) = header_geometry(row, 500.0);
            assert!(!title.intersects(cluster), "overlap at {width} px");
            assert!(
                row.contains_rect(title),
                "title leaves the row at {width} px"
            );
            assert!(
                row.contains_rect(cluster),
                "cluster leaves the row at {width} px"
            );
            assert!(title.left() >= row.left() + original::HEADER_PAD_L - 0.01);
            assert!(cluster.right() <= row.right() - original::HEADER_PAD_R + 0.01);
            width += 1.0;
        }
    }

    /// A long page name must not squeeze the commands out of existence, and a
    /// row too narrow for both halves collapses the cluster instead of
    /// inverting it into a negative rect.
    #[test]
    fn a_long_title_keeps_the_cluster_its_share() {
        let row = header_row(748.0);
        let (title, cluster) = header_geometry(row, 500.0);
        let content = 748.0 - original::HEADER_PAD_L - original::HEADER_PAD_R;
        assert!(
            cluster.width() + 0.01 >= content * 0.5 - original::HEADER_GAP,
            "cluster shrank to {} px",
            cluster.width()
        );
        assert!(
            title.width() <= content * 0.5 + 0.01,
            "title took {} px",
            title.width()
        );
        assert!(title.width() >= original::HEADER_TITLE_MIN_W - 0.01);

        let (_, collapsed) = header_geometry(header_row(60.0), 500.0);
        assert!(!collapsed.is_negative());
        assert_eq!(collapsed.width(), 0.0);
    }

    /// The title is the only part allowed to shrink silently, so the elision
    /// has to keep its promise for every budget, including budgets narrower
    /// than the ellipsis itself.
    #[test]
    fn elided_titles_never_exceed_their_budget() {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let font = crate::theme::font_semibold_in(ui, 15.5);
            let samples = [
                "Processes",
                "Leistung",
                "Startauswirkungen und App-Verlauf",
                "Windows Explorer (explorer.exe)",
                "",
                "…",
            ];
            for text in samples {
                let mut max_w = 0.0_f32;
                while max_w < 120.0 {
                    let elided = elide_to_width(ui, text, &font, max_w);
                    assert!(
                        text_width(ui, &elided, &font) <= max_w + 0.01,
                        "{text:?} at {max_w} px rendered {elided:?}"
                    );
                    assert!(
                        elided.is_empty() || !elided.contains("..."),
                        "{text:?} at {max_w} px used three dots"
                    );
                    max_w += 4.0;
                }
            }
            // A name that fits comes back untouched.
            let fitted = text_width(ui, "Processes", &font);
            assert_eq!(elide_to_width(ui, "Processes", &font, fitted), "Processes");
        });
        out.textures_delta.clear();
    }

    /// The clip is what holds a cluster wider than its share inside its own
    /// half of the row: right-to-left layout keeps walking left instead of
    /// stopping, so without it the buttons repaint over the title.
    #[test]
    fn cluster_painting_stays_inside_its_rect() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 400.0));
        let (_, cluster_rect) = header_geometry(header_row(748.0), 500.0);
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ui| {
                let mut cluster = header_cluster_ui(ui, cluster_rect);
                assert_eq!(cluster.clip_rect(), cluster_rect);
                // Deliberately far too wide for the cluster.
                cluster.painter().text(
                    egui::pos2(cluster_rect.left() - 300.0, cluster_rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    "End task",
                    egui::FontId::proportional(13.0),
                    egui::Color32::WHITE,
                );
            },
        );
        out.textures_delta.clear();
        assert!(!out.shapes.is_empty(), "nothing was painted");
        let mut overflowed = false;
        for clipped in &out.shapes {
            assert!(
                cluster_rect.contains_rect(clipped.clip_rect),
                "paint escaped the cluster: {:?}",
                clipped.clip_rect
            );
            if let egui::Shape::Text(text) = &clipped.shape {
                overflowed |= text.pos.x < cluster_rect.left();
            }
        }
        assert!(overflowed, "the oversized label was not painted at all");
    }
}
