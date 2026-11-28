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

/// Ceiling for the width the page title reserves in a header row.
///
/// The longest page title this app ships is the German "Startauswirkungen",
/// about 133 px at 15.5 px semibold. Capping the reservation near that keeps
/// that name intact while stopping a pathological or unusually long
/// localisation from eating the command row. The 133 px is an approximation,
/// not a promise: the reservation is measured every frame and only clamped by
/// this bound.
const HEADER_TITLE_MAX_W: f32 = 140.0;

/// The command cluster's allotment in one header row: everything right of the
/// title's reservation, inset by `HEADER_PAD_R` — the same 8 px the row used to
/// add as a leading space, so the rightmost command lands where it always did.
///
/// `title_reserve` is the page title's own measured width, clamped to
/// `HEADER_TITLE_MIN_W ..= HEADER_TITLE_MAX_W` by the caller. A fixed floor
/// would elide the page name itself whenever the cluster filled the row, and no
/// single number is right for every font metric and localisation. A reserve is
/// not an estimate of the cluster's width either: the real split is still read
/// back from what the cluster draws.
///
/// A row too narrow for that reservation collapses the cluster onto the right
/// edge instead of producing a rect whose minimum sits past its maximum.
pub(crate) fn header_cluster_rect(row: egui::Rect, title_reserve: f32) -> egui::Rect {
    let right = row.right() - original::HEADER_PAD_R;
    let reserved = row.left() + original::HEADER_PAD_L + title_reserve + original::HEADER_GAP;
    let left = reserved.min(right);
    egui::Rect::from_min_max(egui::pos2(left, row.top()), egui::pos2(right, row.bottom()))
}

/// The page title's rect, given the left edge the command cluster actually used.
///
/// The title takes exactly what the commands left over, minus the gap, so the
/// commands keep their full labels whenever the row can hold them and only then
/// degrade to icons. The priority is deliberate: interactive commands over a
/// decorative page name, and the name elides rather than colliding. Nothing
/// here estimates the cluster's width.
///
/// With the reservation sized to the measured name, a cluster that stays inside
/// its allotment always leaves the full name intact, at every width the cluster
/// can serve. Elision is left for rows so narrow that the cluster overruns its
/// allotment, which is what the cluster's clip is for.
///
/// Both bounds are built so the result can never invert, whatever the row and
/// the reported edge are.
pub(crate) fn header_title_rect(row: egui::Rect, cluster_used_left: f32) -> egui::Rect {
    let left = row.left() + original::HEADER_PAD_L;
    let right_bound = (row.right() - original::HEADER_PAD_R).max(left);
    let right = (cluster_used_left - original::HEADER_GAP)
        .max(left)
        .min(right_bound);
    egui::Rect::from_min_max(egui::pos2(left, row.top()), egui::pos2(right, row.bottom()))
}

/// Gap between the page title and the selection count beside it.
const HEADER_CHIP_GAP: f32 = 10.0;

/// Whether the selection count still fits beside the page title inside a title
/// rect `title_rect_w` wide.
///
/// Every input is measured at the font the row paints with, never estimated: the
/// count is the last thing the row can spare, so it may only appear when the
/// title keeps its own width.
fn chip_fits(title_rect_w: f32, title_w: f32, item_spacing: f32, chip_w: f32) -> bool {
    title_rect_w >= title_w + item_spacing + HEADER_CHIP_GAP + chip_w
}

/// The command cluster's own ui: laid out right to left inside `rect` and
/// clipped to it.
///
/// A right-to-left layout walks leftwards from `rect`'s right edge and never
/// stops at its left edge, so a cluster wider than its share keeps going over
/// the title. The clip is the net that holds every command inside the cluster's
/// own allotment of the row, and it keeps the clipped-away part unclickable too.
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
/// The command cluster draws first into its own rect, clipped to that rect; the
/// page title then gets exactly what the commands left unused. So the commands
/// keep their full labels for as long as the row can hold them, the title
/// elides instead of colliding, and a cluster wider than its allotment is cut
/// off at its own edge instead of painting over the title.
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
    // The title reserves what its own name measures, within bounds: a fixed
    // floor elided the page name whenever the commands filled the row, and no
    // single number is right for every font metric and localisation.
    let title_reserve =
        text_width(ui, title, &title_font).clamp(original::HEADER_TITLE_MIN_W, HEADER_TITLE_MAX_W);
    let cluster_rect = header_cluster_rect(row, title_reserve);

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

    // What the cluster used decides the split, so the commands never give up a
    // label for a title that would have elided anyway. A cluster that drew
    // nothing leaves `Rect::NOTHING` behind, whose left edge is infinite.
    let used_left = cluster.min_rect().left();
    let used_left = if used_left.is_finite() {
        used_left
    } else {
        row.right() - original::HEADER_PAD_R
    };
    let title_rect = header_title_rect(row, used_left);

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
        // What `RichText::size(12.0)` resolves to: the body family at 12 px.
        let chip_font = egui::FontId::proportional(12.0);
        let chip_w = text_width(&title_ui, &chip, &chip_font);
        let gap = title_ui.spacing().item_spacing.x;
        // Priority, top to bottom: the interactive commands, then the page name,
        // then this count. The table already shows which rows are selected, so
        // the count is the first part of the row that can do without.
        if chip_fits(title_rect.width(), title_w, gap, chip_w) {
            title_ui.add_space(HEADER_CHIP_GAP);
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

    // No test here can drive the real `tab_header`: `TaskManApp::new` takes an
    // `eframe::CreationContext`, which needs a window. The geometry helpers and
    // the cluster clip test below are the contract this header is held to, not
    // a stand-in for an end-to-end render.

    fn header_row(width: f32) -> egui::Rect {
        egui::Rect::from_min_size(
            egui::pos2(0.0, 10.0),
            egui::vec2(width, original::HEADER_ROW_H),
        )
    }

    /// The reported defect: the command cluster laid itself out right to left
    /// without a floor and painted over the page title. Both rects now come out
    /// of one split, at every width the window can have and for every reserve the
    /// clamp can produce.
    #[test]
    fn title_and_command_cluster_never_share_a_pixel() {
        let reserves = [original::HEADER_TITLE_MIN_W, 76.0, HEADER_TITLE_MAX_W];
        let mut width = 200.0;
        while width <= 2000.0 {
            for reserve in reserves {
                let row = header_row(width);
                let cluster = header_cluster_rect(row, reserve);
                // Worst case for the title: the cluster used all of its allotment.
                let title = header_title_rect(row, cluster.left());
                assert!(
                    (cluster.right() - (row.right() - original::HEADER_PAD_R)).abs() < 0.01,
                    "cluster right edge moved at {width} px, reserve {reserve}"
                );
                let reserved = row.left() + original::HEADER_PAD_L + reserve + original::HEADER_GAP;
                if reserved <= row.right() - original::HEADER_PAD_R {
                    assert!(
                        cluster.left() >= reserved - 0.01,
                        "title reservation ignored at {width} px, reserve {reserve}"
                    );
                }
                assert!(
                    !title.intersects(cluster),
                    "overlap at {width} px, reserve {reserve}"
                );
                if cluster.width() > 0.0 {
                    assert!(
                        title.right() + original::HEADER_GAP <= cluster.left() + 0.01,
                        "the gap to the cluster shrank at {width} px, reserve {reserve}"
                    );
                }
                assert!(
                    row.contains_rect(title),
                    "title leaves the row at {width} px, reserve {reserve}"
                );
                assert!(
                    row.contains_rect(cluster),
                    "cluster leaves the row at {width} px, reserve {reserve}"
                );
                assert!(
                    !title.is_negative() && !cluster.is_negative(),
                    "negative rect at {width} px, reserve {reserve}"
                );
            }
            width += 1.0;
        }
    }

    /// The headline case: a cluster that fills its whole allotment hands the
    /// title back exactly the width it reserved, whatever that is. At the
    /// reported 748 px row this is what keeps "Processes" — about 76 px at
    /// 15.5 px semibold — from coming out elided as "Process…".
    #[test]
    fn a_filled_cluster_still_leaves_the_title_its_reserved_width() {
        let mut reserve = original::HEADER_TITLE_MIN_W;
        while reserve <= HEADER_TITLE_MAX_W {
            let row = header_row(748.0);
            let cluster = header_cluster_rect(row, reserve);
            let allotment = 748.0
                - original::HEADER_PAD_L
                - reserve
                - original::HEADER_GAP
                - original::HEADER_PAD_R;
            assert!(
                (cluster.width() - allotment).abs() < 0.01,
                "allotment for reserve {reserve} was {}",
                cluster.width()
            );
            let title = header_title_rect(row, cluster.left());
            assert_eq!(
                title.width(),
                reserve,
                "a full cluster left the title {} instead of {reserve} px",
                title.width()
            );
            reserve += 1.0;
        }

        // The reported window size, spelled out: 748 px row, ~76 px name.
        let row = header_row(748.0);
        let title = header_title_rect(row, header_cluster_rect(row, 76.0).left());
        assert_eq!(
            title.width(),
            76.0,
            "the reported window would still elide \"Processes\""
        );
    }

    /// At the reported window size (960 px window, 212 px sidebar, 748 px row) a
    /// labelled Processes cluster is about 630 px wide. That has to fit inside
    /// the cluster's allotment and still leave room for "Processes" beside it.
    #[test]
    fn a_wide_cluster_still_leaves_the_title_its_minimum() {
        let row = header_row(748.0);
        let reserve = 76.0;
        let used = 630.0;
        let cluster_used_left = row.right() - original::HEADER_PAD_R - used;
        assert!(
            header_cluster_rect(row, reserve).width() >= used,
            "the allotment cannot hold a labelled cluster"
        );
        let title = header_title_rect(row, cluster_used_left);
        assert!(title.width() >= reserve, "title got {} px", title.width());
        assert!(title.right() + original::HEADER_GAP <= cluster_used_left + 0.01);
    }

    /// A cluster that drew nothing — every Windows-only command is absent on
    /// Linux, or everything is disabled — leaves the title the whole content
    /// width, minus the gap the row always keeps.
    #[test]
    fn an_empty_cluster_leaves_the_title_the_content_width() {
        let row = header_row(748.0);
        let content = 748.0 - original::HEADER_PAD_L - original::HEADER_PAD_R;
        let title = header_title_rect(row, row.right() - original::HEADER_PAD_R);
        assert!(
            title.width() + 0.01 >= content - original::HEADER_GAP,
            "title got only {} px",
            title.width()
        );
        assert!(
            title.right() + original::HEADER_GAP <= row.right() - original::HEADER_PAD_R + 0.01
        );
    }

    /// When the cluster overruns even its allotment the title gives way, but it
    /// never inverts and never reaches the commands; the clip holds the rest.
    #[test]
    fn an_overflowing_cluster_shrinks_the_title_without_inverting_it() {
        let row = header_row(748.0);
        let reserve = 76.0;
        let cluster = header_cluster_rect(row, reserve);
        // 20 px wider than the allotment, so the cluster's left edge moves out.
        let title = header_title_rect(row, cluster.left() - 20.0);
        assert!(title.width() < reserve, "the title should have given way");
        assert!(!title.is_negative());
        assert!(!title.intersects(cluster));
        assert!(
            row.contains_rect(title),
            "title leaves the row when squeezed"
        );
        assert!(
            title.right() + original::HEADER_GAP <= cluster.left(),
            "a squeezed title still keeps its gap"
        );

        // A cluster that used the whole row cannot invert the title either.
        let squashed = header_title_rect(row, row.left() - 500.0);
        assert!(!squashed.is_negative());
        assert_eq!(squashed.width(), 0.0);
    }

    /// A row narrower than the reservation cannot hold it at all: both rects must
    /// stay non-negative, inside the row and disjoint instead of inverting.
    #[test]
    fn a_degenerate_row_collapses_without_panicking() {
        let row = header_row(60.0);
        let cluster = header_cluster_rect(row, original::HEADER_TITLE_MIN_W);
        let title = header_title_rect(row, cluster.left());
        assert_eq!(cluster.width(), 0.0);
        assert!(!title.is_negative() && !cluster.is_negative());
        assert!(row.contains_rect(title), "title leaves a 60 px row");
        assert!(row.contains_rect(cluster), "cluster leaves a 60 px row");
        assert!(!title.intersects(cluster));
    }

    /// The selection count yields before the page title does: it appears beside
    /// the name when the room is there (a 2560 px window has plenty) and drops
    /// out while the name keeps its full width when it is not.
    #[test]
    fn the_selection_count_yields_before_the_page_title() {
        // Measured width of a fully labelled Processes cluster.
        const CLUSTER_USED: f32 = 640.0;
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let title_font = crate::theme::font_semibold_in(ui, 15.5);
            let title_w = text_width(ui, "Processes", &title_font);
            let chip_w = text_width(ui, "3 selected", &egui::FontId::proportional(12.0));
            let gap = ui.spacing().item_spacing.x;

            // 2560 px window minus the 212 px sidebar.
            let wide_row = header_row(2348.0);
            let wide_used_left = wide_row.right() - original::HEADER_PAD_R - CLUSTER_USED;
            let wide = header_title_rect(wide_row, wide_used_left);
            assert!(
                chip_fits(wide.width(), title_w, gap, chip_w),
                "no count beside the title in a 2348 px row"
            );

            let mut dropped_with_full_title = false;
            let mut width = 400.0;
            while width <= 2000.0 {
                let row = header_row(width);
                let used_left = row.right() - original::HEADER_PAD_R - CLUSTER_USED;
                let title = header_title_rect(row, used_left);
                if chip_fits(title.width(), title_w, gap, chip_w) {
                    assert!(
                        title.width() >= title_w,
                        "count drawn while the title was squeezed at {width} px"
                    );
                } else if title.width() >= title_w {
                    dropped_with_full_title = true;
                }
                width += 1.0;
            }
            assert!(
                dropped_with_full_title,
                "no width drops the count while the title keeps its width"
            );
        });
        out.textures_delta.clear();
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

    /// The clip is what holds a cluster wider than its allotment inside that
    /// allotment: right-to-left layout keeps walking left instead of stopping,
    /// so without it the buttons repaint over the title.
    #[test]
    fn cluster_painting_stays_inside_its_rect() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 400.0));
        let cluster_rect = header_cluster_rect(header_row(748.0), original::HEADER_TITLE_MIN_W);
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
