//! App history tab: "App-Verlauf" — caption with since-date, clear-history
//! link, and the CPU-Zeit/Netzwerk/Benachrichtigungen heat table.

use eframe::egui;
use std::cmp::Ordering;
use tm_core::format;
use tm_core::i18n::{self, K};

use crate::app::TaskManApp;
use crate::search;
use crate::theme;
use crate::widgets::menu;
use crate::widgets::tablekit::{self, TmColumn};

fn columns() -> Vec<TmColumn> {
    vec![
        // Only metrics we actually measure. The old fake "Notifications"
        // column (hard-coded "0 MB") was removed until a real Windows data
        // source exists (implement.md §16.8).
        TmColumn::text("name", i18n::tr(K::ColName), 340.0),
        TmColumn::num("cpu", i18n::tr(K::ColCpuTime), 150.0),
        TmColumn::num("net", i18n::tr(K::ColNetwork), 140.0),
    ]
}

#[derive(Debug, Clone)]
struct Row {
    name: String,
    cpu_seconds: f64,
    network_bytes: u64,
    network_available: bool,
}

fn compare_rows(a: &Row, b: &Row, sort: tablekit::SortState) -> Ordering {
    if sort.column == 2 && a.network_available != b.network_available {
        // Missing telemetry stays last in either direction.
        return b.network_available.cmp(&a.network_available);
    }
    let primary = match sort.column {
        0 => tablekit::cmp_ignore_case(&a.name, &b.name),
        1 => a
            .cpu_seconds
            .partial_cmp(&b.cpu_seconds)
            .unwrap_or(Ordering::Equal),
        _ => a.network_bytes.cmp(&b.network_bytes),
    };
    tablekit::directed(primary, sort.ascending)
        .then_with(|| tablekit::cmp_ignore_case(&a.name, &b.name))
}

/// egui temp-data key of the keyboard selection: the row OWNER (the display
/// name, the same identity `TmTable::row` receives). The page has no other
/// selection state, and the entry is touched every frame the page is shown.
const SELECTION_KEY: &str = "tm-apphistory-selection";

fn selected_name(ctx: &egui::Context) -> Option<String> {
    ctx.data(|d| d.get_temp::<Option<String>>(egui::Id::new(SELECTION_KEY)))
        .unwrap_or(None)
}

fn set_selected_name(ctx: &egui::Context, name: Option<String>) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(SELECTION_KEY), name));
}

/// Arrow/Home/End/Page selection movement over the displayed rows. The
/// selection is returned for the caller to store; `None` means "no keyboard
/// movement this frame" and leaves the current selection alone. `dialog_open`
/// (see `TaskManApp::modal_open`) stands the page layer down while a dialog
/// owns the keyboard.
fn keyboard_selection(
    ctx: &egui::Context,
    rows: &[Row],
    selected: Option<&str>,
    dialog_open: bool,
) -> Option<String> {
    if !search::nav_gate(ctx, dialog_open) {
        return None;
    }
    let page_rows = tablekit::page_rows(ctx, "apphistory", tablekit::ROW_H).unwrap_or_else(|| {
        (ctx.content_rect().height() / tablekit::ROW_H)
            .floor()
            .max(1.0) as usize
    });
    let current = selected.and_then(|name| rows.iter().position(|r| r.name == name));
    if let Some(nav) = search::list_nav(ctx, dialog_open)
        .filter(|_| !tablekit::header_has_focus(ctx, "apphistory"))
        && let Some(next) = search::moved_index(rows.len(), current, nav, page_rows)
        && let Some(row) = rows.get(next)
    {
        // One-shot scroll parked under the row-owner identity, resolved to a
        // flat row index per frame by the table call below.
        tablekit::request_row_scroll(ctx, "apphistory", tablekit::stable_key(row.name.as_str()));
        return Some(row.name.clone());
    }
    None
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);

    crate::app_ui::tab_header(
        app,
        ui,
        &pal,
        |_app, _ui| {},
        |app, ui| {
            if menu::item(ui, i18n::tr(K::RefreshNow)).clicked() {
                app.refresh_all();
                ui.close();
            }
        },
    );

    let since = format::format_date(app.app_history_db.since_epoch_s());
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "{} {since} {}",
                    i18n::tr(K::HistorySinceLine),
                    i18n::tr(K::HistoryForAccounts)
                ))
                .size(13.0),
            );
            ui.label(
                egui::RichText::new(i18n::tr(K::HistoryLocalNote))
                    .size(12.0)
                    .color(pal.text_dim),
            );
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new(i18n::tr(K::ClearHistoryLink))
                            .size(13.0)
                            .color(pal.accent),
                    )
                    .frame(false),
                )
                .clicked()
            {
                // Clearing wipes the whole database; park behind a confirm
                // like every other destructive command.
                app.pending_app_history_clear = true;
            }
        });
    });
    ui.add_space(6.0);

    let q = crate::search::Query::new(&app.search);
    let db_names = app.app_history_db.display_name_map();
    let mut rows: Vec<Row> = app
        .app_history_db
        .entries()
        .iter()
        .map(|(k, v)| {
            let shown = db_names.get(k).cloned().unwrap_or_else(|| k.clone());
            Row {
                name: shown,
                cpu_seconds: v.cpu_seconds,
                network_bytes: v.network_bytes,
                network_available: v.network_available,
            }
        })
        .filter(|row| q.matches_any([row.name.as_str()]))
        .collect();
    rows.sort_by(|a, b| compare_rows(a, b, app.app_history_sort));

    let mut table = app.make_table("apphistory", columns());
    let mut fit = [
        tablekit::text_width(ui, table.cols[0].label, tablekit::FONT_HDR_LABEL) + 28.0,
        tablekit::text_width(ui, table.cols[1].label, tablekit::FONT_HDR_LABEL) + 28.0,
        tablekit::text_width(ui, table.cols[2].label, tablekit::FONT_HDR_LABEL) + 28.0,
    ];
    for row in &rows {
        fit[0] = fit[0].max(tablekit::text_width(ui, &row.name, tablekit::FONT_ROW) + 66.0);
        fit[1] = fit[1].max(
            tablekit::text_width(
                ui,
                &format::format_cpu_time(row.cpu_seconds),
                tablekit::FONT_ROW,
            ) + 22.0,
        );
        let network = if row.network_available {
            format::format_bytes_loc(row.network_bytes)
        } else {
            "—".into()
        };
        fit[2] = fit[2].max(tablekit::text_width(ui, &network, tablekit::FONT_ROW) + 22.0);
    }
    table.apply_auto_fit(fit);

    // Per-column maxima over the whole model BEFORE virtualization
    // (audit P0.2) — CPU time and network traffic each highlight their own
    // top consumer.
    let max_cpu = rows
        .iter()
        .map(|row| row.cpu_seconds)
        .fold(0.0f64, f64::max);
    let max_net = rows
        .iter()
        .filter(|row| row.network_available)
        .map(|row| row.network_bytes as f64)
        .fold(0.0f64, f64::max);

    let avail = crate::widgets::tablekit::table_avail(ui);
    // Arrow/Home/End/Page movement of the visible selection, stored per
    // frame in the page's temp-data key.
    if let Some(name) = keyboard_selection(
        ui.ctx(),
        &rows,
        selected_name(ui.ctx()).as_deref(),
        app.modal_open(),
    ) {
        set_selected_name(ui.ctx(), Some(name));
    }
    let focus_row = tablekit::take_row_scroll(ui.ctx(), "apphistory").and_then(|key| {
        rows.iter()
            .position(|r| tablekit::stable_key(r.name.as_str()) == key)
    });
    let clicked = crate::widgets::tablekit::scrolled_rows(
        "apphistory",
        ui,
        &pal,
        &mut table,
        avail,
        Some((app.app_history_sort.column, app.app_history_sort.ascending)),
        None,
        rows.len(),
        (!q.is_empty()).then_some(i18n::tr(K::NoMatches)),
        focus_row,
        None,
        |ui, table, _avail, _content_w, range| {
            for ri in range {
                let Some(row) = rows.get(ri) else {
                    continue;
                };
                // Read the selection HERE, not from a snapshot taken before
                // the table ran: the row closure itself stores a click, so
                // only a live read keeps every already-painted row of this
                // frame consistent with it.
                let is_selected = selected_name(ui.ctx()).as_deref() == Some(row.name.as_str());
                let (rect, resp) = table.row(ui, &pal, is_selected, row.name.as_str());
                table.describe_row(ui, &resp, &row.name, ri);
                if resp.clicked() {
                    set_selected_name(ui.ctx(), Some(row.name.clone()));
                    // The row painted its fill BEFORE this frame's click was
                    // known, so the clicked row would light up one frame late.
                    // Repaint it now and re-arm the overlay so the heat band
                    // (painted later in this same row pass) re-applies it.
                    table.repaint_row_selected(ui, &pal, rect);
                }
                table.icon_cell(ui, rect, None, pal.accent);
                let name_rect = table.col_rect(0, rect);
                ui.painter_at(name_rect).text(
                    egui::Pos2::new(name_rect.left() + 56.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &row.name,
                    egui::FontId::proportional(tablekit::FONT_ROW),
                    pal.text,
                );
                let cells = vec![
                    tablekit::HeatCell::new(
                        tablekit::norm(row.cpu_seconds, max_cpu),
                        format::format_cpu_time(row.cpu_seconds),
                    ),
                    tablekit::HeatCell::new(
                        if row.network_available {
                            tablekit::norm(row.network_bytes as f64, max_net)
                        } else {
                            0.0
                        },
                        if row.network_available {
                            format::format_bytes_loc(row.network_bytes)
                        } else {
                            "—".into()
                        },
                    ),
                ];
                table.heat_cells(ui, &pal, rect, 1, &cells);
                // A "—" the user can hover explains itself (same reasons the
                // Users tab gives); an unexplained dash reads as a bug.
                if !row.network_available
                    && let Some(tip) =
                        crate::tabs::value_columns::unavailable_tip(crate::tabs::value_columns::NET)
                {
                    let _ = resp.on_hover_text(tip);
                }
            }
        },
    );
    if let Some(column) = clicked {
        app.app_history_sort.clicked(column, column != 0);
        let ids = ["name", "cpu", "net", "notif"];
        app.persist_sort("apphistory", ids[column], app.app_history_sort.ascending);
    }
    app.persist_table(&table);
}

/// Confirmation for "Delete usage history" (`show` parks the click here
/// instead of wiping the database right away). The safe action owns the
/// keyboard default.
pub fn clear_history_dialog(app: &mut TaskManApp, ctx: &egui::Context, pal: &theme::Palette) {
    if !app.pending_app_history_clear {
        return;
    }
    let restore_id = egui::Id::new("apphistory-clear-restore-focus");
    let first_frame = ctx
        .data(|d| d.get_temp::<Option<egui::Id>>(restore_id))
        .is_none();
    if first_frame {
        // Remember what held keyboard focus before the dialog took it, so
        // closing can hand it back.
        let captured = crate::app_ui::capture_focus(ctx);
        ctx.data_mut(|d| d.insert_temp(restore_id, captured));
    }
    let focus_id = egui::Id::new("apphistory-clear-focus-primary");
    let mut open = true;
    let keys = crate::app_ui::consume_dialog_keys(ctx, true);
    let mut focused: bool = ctx.data(|d| d.get_temp(focus_id)).unwrap_or(false);
    focused = crate::app_ui::update_end_task_dialog_focus(
        focused,
        keys.tab,
        keys.shift_tab,
        keys.left,
        keys.right,
    );
    let key_decision = crate::app_ui::dialog_key_decision(keys, focused, true);
    let mut clicked = crate::app_ui::DialogButtonClick::None;
    egui::Window::new(i18n::tr(K::ClearHistoryLink))
        .modal(true)
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(i18n::tr(K::ClearHistoryConfirm));
            ui.add_space(8.0);
            clicked = crate::app_ui::dialog_button_row(
                ui,
                ctx,
                pal,
                i18n::tr(K::Cancel),
                Some((i18n::tr(K::ClearHistoryLink), true)),
                &mut focused,
            );
        });
    ctx.data_mut(|d| d.insert_temp(focus_id, focused));
    let cancel = !open
        || matches!(key_decision, Some(crate::app_ui::DialogDecision::Safe))
        || matches!(clicked, crate::app_ui::DialogButtonClick::Safe);
    let confirm = matches!(key_decision, Some(crate::app_ui::DialogDecision::Primary))
        || matches!(clicked, crate::app_ui::DialogButtonClick::Primary);
    if cancel || confirm {
        // Whichever way it resolved, hand keyboard focus back to what held
        // it before the dialog took it.
        let saved = ctx
            .data(|d| d.get_temp::<Option<egui::Id>>(restore_id))
            .flatten();
        crate::app_ui::restore_focus(ctx, saved);
        ctx.data_mut(|d| d.remove_temp::<Option<egui::Id>>(restore_id));
        ctx.data_mut(|d| d.remove_temp::<bool>(focus_id));
        app.pending_app_history_clear = false;
    }
    if confirm {
        app.app_history_db.clear();
        app.shared.toast(i18n::tr(K::HistoryCleared));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_network_sorts_last_in_both_directions() {
        let available = Row {
            name: "available".into(),
            cpu_seconds: 0.0,
            network_bytes: 0,
            network_available: true,
        };
        let missing = Row {
            name: "missing".into(),
            cpu_seconds: 0.0,
            network_bytes: 0,
            network_available: false,
        };
        for ascending in [true, false] {
            assert_eq!(
                compare_rows(&available, &missing, tablekit::SortState::new(2, ascending)),
                Ordering::Less
            );
        }
    }

    fn nav_rows() -> Vec<Row> {
        ["a", "b", "c"]
            .into_iter()
            .map(|name| Row {
                name: name.to_owned(),
                cpu_seconds: 1.0,
                network_bytes: 0,
                network_available: false,
            })
            .collect()
    }

    /// One frame carrying a single pressed key.
    fn key_ctx(ctx: &egui::Context, key: egui::Key) {
        let event = egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        };
        let mut out = ctx.run_ui(
            egui::RawInput {
                events: vec![event],
                ..Default::default()
            },
            |ui| {
                search::set_active_content(ui.ctx(), "apphistory");
                ui.ctx().memory_mut(|memory| {
                    memory.request_focus(search::content_focus_id("apphistory"))
                });
                ui.interact(
                    egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 20.0)),
                    search::content_focus_id("apphistory"),
                    egui::Sense::focusable_noninteractive(),
                );
            },
        );
        out.textures_delta.clear();
    }

    /// One frame with no events (egui keeps the previous frame's input until
    /// a new frame begins).
    fn blank_ctx(ctx: &egui::Context) {
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
    }

    /// The page's keyboard selection walks the displayed rows by the name
    /// owner key, starts at the first row without a selection, and parks a
    /// one-shot scroll request under the identity it moved to.
    #[test]
    fn keyboard_selection_walks_rows_and_parks_a_scroll_request() {
        let ctx = egui::Context::default();
        let rows = nav_rows();
        // No key: no movement, and the existing selection is left alone.
        assert_eq!(keyboard_selection(&ctx, &rows, None, false), None);

        // From nothing, ArrowDown starts at the first row.
        key_ctx(&ctx, egui::Key::ArrowDown);
        assert_eq!(
            keyboard_selection(&ctx, &rows, None, false).as_deref(),
            Some("a")
        );
        set_selected_name(&ctx, Some("a".to_owned()));

        // From "a", ArrowDown moves to "b" and parks the scroll under it.
        key_ctx(&ctx, egui::Key::ArrowDown);
        assert_eq!(
            keyboard_selection(&ctx, &rows, Some("a"), false).as_deref(),
            Some("b")
        );
        assert_eq!(
            tablekit::take_row_scroll(&ctx, "apphistory"),
            Some(tablekit::stable_key("b")),
            "the scroll request must carry the row-owner identity"
        );
        // The request is one-shot: taking it consumes it.
        assert_eq!(tablekit::take_row_scroll(&ctx, "apphistory"), None);

        // The first row stays put on ArrowUp; End jumps to the last.
        blank_ctx(&ctx);
        assert_eq!(
            keyboard_selection(&ctx, &rows, Some("a"), false).as_deref(),
            None,
            "no keypress this frame means no movement"
        );
        key_ctx(&ctx, egui::Key::End);
        assert_eq!(
            keyboard_selection(&ctx, &rows, Some("a"), false).as_deref(),
            Some("c")
        );
        key_ctx(&ctx, egui::Key::ArrowUp);
        assert_eq!(
            keyboard_selection(&ctx, &rows, Some("c"), false).as_deref(),
            Some("b")
        );
    }
}
