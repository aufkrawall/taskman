//! App history tab: "App-Verlauf" — caption with since-date, clear-history
//! link, and the CPU-Zeit/Netzwerk/Benachrichtigungen heat table.

use eframe::egui;
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
    let mut rows: Vec<(String, f64, u64)> = app
        .app_history_db
        .entries()
        .iter()
        .map(|(k, v)| {
            let shown = db_names.get(k).cloned().unwrap_or_else(|| k.clone());
            (shown, v.cpu_seconds, v.network_bytes)
        })
        .filter(|(name, _, _)| q.matches_any([name.as_str()]))
        .collect();
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut table = app.make_table("apphistory", columns());
    let mut fit = [
        tablekit::text_width(ui, table.cols[0].label, tablekit::FONT_HDR_LABEL) + 28.0,
        tablekit::text_width(ui, table.cols[1].label, tablekit::FONT_HDR_LABEL) + 28.0,
        tablekit::text_width(ui, table.cols[2].label, tablekit::FONT_HDR_LABEL) + 28.0,
    ];
    for (name, cpu_s, net_b) in &rows {
        fit[0] = fit[0].max(tablekit::text_width(ui, name, tablekit::FONT_ROW) + 66.0);
        fit[1] = fit[1].max(
            tablekit::text_width(ui, &format::format_cpu_time(*cpu_s), tablekit::FONT_ROW) + 22.0,
        );
        fit[2] = fit[2].max(
            tablekit::text_width(ui, &format::format_bytes_loc(*net_b), tablekit::FONT_ROW) + 22.0,
        );
    }
    for (i, width) in fit.into_iter().enumerate() {
        table.set_auto_fit_width(i, width.ceil());
    }

    // Per-column maxima over the whole model BEFORE virtualization
    // (audit P0.2) — CPU time and network traffic each highlight their own
    // top consumer.
    let max_cpu = rows.iter().map(|r| r.1).fold(0.0f64, f64::max);
    let max_net = rows.iter().map(|r| r.2 as f64).fold(0.0f64, f64::max);

    let avail = crate::widgets::tablekit::table_avail(ui);
    crate::widgets::tablekit::scrolled_rows(
        "apphistory",
        ui,
        &pal,
        &mut table,
        avail,
        None,
        None,
        rows.len(),
        None,
        |ui, table, _avail, _content_w, range| {
            for ri in range {
                let Some((name, cpu_s, net_b)) = rows.get(ri) else {
                    continue;
                };
                let (rect, resp) = table.row(ui, &pal, false);
                table.icon_cell(ui, rect, None, pal.accent);
                let name_rect = table.col_rect(0, rect);
                ui.painter_at(name_rect).text(
                    egui::Pos2::new(name_rect.left() + 56.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    name,
                    egui::FontId::proportional(tablekit::FONT_ROW),
                    pal.text,
                );
                let cells = vec![
                    tablekit::HeatCell::new(
                        tablekit::norm(*cpu_s, max_cpu),
                        format::format_cpu_time(*cpu_s),
                    ),
                    tablekit::HeatCell::new(
                        tablekit::norm(*net_b as f64, max_net),
                        format::format_bytes_loc(*net_b),
                    ),
                ];
                table.heat_cells(ui, &pal, rect, 1, &cells, true);
                let _ = resp;
            }
        },
    );
    app.persist_table(&table);
}
