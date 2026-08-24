//! App history tab: cumulative CPU time / network per application since we
//! started tracking (persisted across runs).

use eframe::egui;

use crate::app::TaskManApp;
use crate::theme;

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);

    let since_epoch = app.app_history_db.since_epoch_s();
    let since_text = chrono_fmt_local(since_epoch);

    ui.heading("App-Verlauf");
    ui.label(
        egui::RichText::new(format!(
            "Ressourcennutzung seit {since_text} — CPU-Zeit und Netzwerk werden von Task Manager selbst gemessen."
        ))
        .size(12.0)
        .color(pal.text_dim),
    );
    ui.add_space(4.0);
    ui.separator();

    let entries: Vec<(String, f64, u64)> = app
        .app_history_db
        .entries()
        .iter()
        .map(|(k, v)| (k.clone(), v.cpu_seconds, v.network_bytes))
        .collect();

    let total_cpu: f64 = entries.iter().map(|e| e.1).sum();
    ui.label(format!(
        "Gesamte CPU-Zeit aller Apps: {}",
        tm_core::format::format_cpu_time(total_cpu)
    ));
    ui.separator();

    let mut rows = entries;
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    egui::ScrollArea::vertical()
        .id_salt("apphistory")
        .show(ui, |ui| {
            egui::Grid::new("apphist-header")
                .num_columns(3)
                .spacing([20.0, 4.0])
                .show(ui, |ui| {
                    for h in ["App", "CPU-Zeit", "Netzwerk"] {
                        ui.label(egui::RichText::new(h).size(11.5).color(pal.text_dim));
                    }
                    ui.end_row();
                });
            ui.separator();
            egui::Grid::new("apphist-rows")
                .num_columns(3)
                .spacing([20.0, 1.0])
                .show(ui, |ui| {
                    for (name, cpu_s, net_b) in rows.iter().take(500) {
                        ui.monospace(name);
                        ui.label(format!(
                            "{}  ({:.1} %)",
                            tm_core::format::format_cpu_time(*cpu_s),
                            pct_of(*cpu_s, total_cpu)
                        ));
                        ui.label(tm_core::format::format_bytes(*net_b));
                        ui.end_row();
                    }
                });
            ui.add_space(20.0);
        });
}

fn pct_of(v: f64, total: f64) -> f64 {
    if total > 0.0 { v / total * 100.0 } else { 0.0 }
}

/// Format an epoch timestamp as local date "dd.mm.yyyy hh:mm".
fn chrono_fmt_local(epoch_s: i64) -> String {
    // Avoid a chrono dependency: compute UTC date via days math.
    let secs = epoch_s;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    // Civil-from-days algorithm (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{d:02}.{mo:02}.{y} {h:02}:{m:02}")
}
