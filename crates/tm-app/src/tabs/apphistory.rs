//! App history tab: "App-Verlauf" — caption with since-date, clear-history
//! link, and the CPU-Zeit/Netzwerk/Benachrichtigungen heat table.

use eframe::egui;
use std::cmp::Ordering;
use tm_core::format;
use tm_core::i18n::{self, K};

use crate::app::TaskManApp;
use crate::theme;
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
        0 => a
            .name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase()),
        1 => a
            .cpu_seconds
            .partial_cmp(&b.cpu_seconds)
            .unwrap_or(Ordering::Equal),
        _ => a.network_bytes.cmp(&b.network_bytes),
    };
    let primary = if sort.ascending {
        primary
    } else {
        primary.reverse()
    };
    primary.then_with(|| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    })
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);

    crate::app_ui::tab_header(
        app,
        ui,
        &pal,
        |_app, _ui| {},
        |_app, ui| {
            if ui.button(i18n::tr(K::RefreshNow)).clicked() {
                _app.refresh_all();
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
                app.app_history_db.clear();
                app.shared.toast(i18n::tr(K::HistoryCleared));
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
    for (i, width) in fit.into_iter().enumerate() {
        table.set_auto_fit_width(i, width.ceil());
    }

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
    let clicked = crate::widgets::tablekit::scrolled_rows(
        "apphistory",
        ui,
        &pal,
        &mut table,
        avail,
        Some((app.app_history_sort.column, app.app_history_sort.ascending)),
        None,
        rows.len(),
        None,
        |ui, table, _avail, _content_w, range| {
            for ri in range {
                let Some(row) = rows.get(ri) else {
                    continue;
                };
                let (rect, resp) = table.row(ui, &pal, false);
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
                let _ = resp;
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
}
