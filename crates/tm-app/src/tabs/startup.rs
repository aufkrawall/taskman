//! Startup apps tab: "Autostart von Apps" — Name/Herausgeber/Status/
//! Startauswirkung table with "Letzte BIOS-Zeit" top right and the
//! Aktivieren/Deaktivieren/Eigenschaften command bar.

use eframe::egui;
use std::time::{Duration, Instant};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::{StartupImpact, StartupItem};

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::theme;
use crate::widgets::tablekit::TmColumn;

fn columns() -> Vec<TmColumn> {
    vec![
        TmColumn::text("name", i18n::tr(K::ColName), 0.0),
        TmColumn::text("pub", i18n::tr(K::ColPublisher), 240.0),
        TmColumn::text("status", i18n::tr(K::ColStatus), 140.0),
        TmColumn::text("impact", i18n::tr(K::ColImpact), 150.0),
    ]
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);

    // Lazy refresh in the background (registry + folder scan off the UI thread).
    {
        let stale = {
            let guard = tm_core::sync::lock(&app.shared.startup_cache);
            match guard.as_ref() {
                Some((_, t)) => t.elapsed() > Duration::from_secs(10),
                None => true,
            }
        };
        if stale && app.shared.startup_fetch.begin() {
            let cache = app.shared.startup_cache.clone();
            let toasts = app.shared.toasts.clone();
            let done = app.shared.startup_fetch.flag();
            let actions = app.actions.clone();
            let _ = std::thread::Builder::new()
                .name("tm-startup-fetch".into())
                .spawn(move || {
                    let items = actions.list_startup();
                    if let Err(e) = &items {
                        crate::app::toast_from(
                            &toasts,
                            i18n::trf(K::StartupUnavailable, &[&e.to_string()]),
                        );
                    }
                    *tm_core::sync::lock(&cache) =
                        Some((items.unwrap_or_default(), Instant::now()));
                    done.store(false, std::sync::atomic::Ordering::Relaxed);
                });
        }
    }

    let selected_idx = app.selected_startup_idx;
    crate::app_ui::tab_header(
        app,
        ui,
        &pal,
        |app, ui| {
            let sel: Option<StartupItem> = selected_idx.and_then(|i| {
                let guard = tm_core::sync::lock(&app.shared.startup_cache);
                guard.as_ref().and_then(|(v, _)| v.get(i)).cloned()
            });
            let can_enable = sel.as_ref().is_some_and(|s| !s.enabled);
            let can_disable = sel.as_ref().is_some_and(|s| s.enabled);
            if crate::app_ui::cmd_button(ui, &pal, Icon::Check, i18n::tr(K::EnableCmd), can_enable)
            {
                toggle_selected(app, true);
            }
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::SlashCircle,
                i18n::tr(K::DisableCmd),
                can_disable,
            ) {
                toggle_selected(app, false);
            }
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::Properties,
                i18n::tr(K::Properties),
                sel.is_some(),
            ) {
                app.startup_props = sel.clone();
            }
            let _ = &sel;
        },
        |_app, ui| {
            if ui.button(i18n::tr(K::RefreshNow)).clicked() {
                ui.close();
            }
        },
    );

    // "Letzte BIOS-Zeit:  17,0 Sekunden" — top right, like TM.
    if let Some(ms) = app.actions.last_bios_time_ms() {
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::hover());
        let text = format!(
            "{}   {} {}",
            i18n::tr(K::LastBiosTime),
            format::format_seconds(ms as f64 / 1000.0),
            i18n::tr(K::SecondsSuffix)
        );
        ui.painter().text(
            egui::Pos2::new(rect.right() - 16.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            text,
            egui::FontId::proportional(12.5),
            pal.text,
        );
    }

    // Clone the Arc so the guard never borrows from `app` itself (the
    // closures below need `&mut TaskManApp`).
    let cache_arc = app.shared.startup_cache.clone();
    let mut guard = tm_core::sync::lock(&cache_arc);
    let Some((items, _)) = guard.as_mut() else {
        return;
    };

    let q = app.search.trim().to_lowercase();
    let mut table = app.make_table("startup", columns(), 340.0);
    let avail = crate::widgets::tablekit::table_avail(ui);
    crate::widgets::tablekit::scrolled_table(
        "startup",
        ui,
        &pal,
        &mut table,
        avail,
        None,
        None,
        |ui, table, avail, _content_w| {
            for (i, item) in items.iter_mut().enumerate() {
                if !q.is_empty() && !item.name.to_lowercase().contains(&q) {
                    continue;
                }
                let selected = app.selected_startup_idx == Some(i);
                let (rect, resp) = table.row(ui, &pal, avail, selected);

                // Icon: real shell icon from the command's executable.
                let exe = exe_from_command(&item.command);
                let tex = exe
                    .as_deref()
                    .and_then(|p| app.shared.icons.get(ui.ctx(), app.actions.as_ref(), p, 4));
                table.icon_cell(ui, rect, tex.as_ref(), pal.accent);
                let name_rect = table.col_rect(0, avail, rect);
                ui.painter().text(
                    egui::Pos2::new(name_rect.left() + 56.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &item.name,
                    egui::FontId::proportional(12.5),
                    pal.text,
                );

                table.text_cell(
                    ui,
                    avail,
                    rect,
                    1,
                    item.publisher.as_deref().unwrap_or(""),
                    &pal,
                    false,
                );
                table.text_cell(
                    ui,
                    avail,
                    rect,
                    2,
                    if item.enabled {
                        i18n::tr(K::EnabledWord)
                    } else {
                        i18n::tr(K::DisabledWord)
                    },
                    &pal,
                    false,
                );
                table.text_cell(
                    ui,
                    avail,
                    rect,
                    3,
                    impact_label(app.lang(), item.impact),
                    &pal,
                    false,
                );

                if resp.clicked() {
                    app.selected_startup_idx = Some(i);
                }
                resp.context_menu(|ui| {
                    ui.set_min_width(180.0);
                    let label = if item.enabled {
                        i18n::tr(K::DisableCmd)
                    } else {
                        i18n::tr(K::EnableCmd)
                    };
                    if ui.button(label).clicked() {
                        let new_enabled = !item.enabled;
                        match app
                            .actions
                            .set_startup_enabled(&item.id, &item.location, new_enabled)
                        {
                            Ok(()) => {
                                item.enabled = new_enabled;
                                app.shared.toast(if new_enabled {
                                    i18n::tr(K::EnabledWord)
                                } else {
                                    i18n::tr(K::DisabledWord)
                                });
                            }
                            Err(e) => app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()])),
                        }
                        ui.close();
                    }
                    if ui.button(i18n::tr(K::Properties)).clicked() {
                        app.startup_props = Some(item.clone());
                        ui.close();
                    }
                    if ui.button(i18n::tr(K::OnlineSearch)).clicked() {
                        let url = format!(
                            "https://www.bing.com/search?q={}",
                            urlencoding_lite(&item.name)
                        );
                        if let Err(e) = app.actions.open_url(&url) {
                            app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()]));
                        }
                        ui.close();
                    }
                    if ui.button(i18n::tr(K::OpenFileLocation)).clicked()
                        && let Some(exe) = exe_from_command(&item.command)
                        && let Err(e) = app.actions.open_file_location(&exe)
                    {
                        app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()]));
                    }
                });
            }
            ui.add_space(12.0);
        },
    );
    app.persist_table(&table);
}

fn toggle_selected(app: &mut TaskManApp, enable: bool) {
    let guard = app.shared.startup_cache.clone();
    let mut cache = tm_core::sync::lock(&guard);
    if let Some((items, _)) = cache.as_mut()
        && let Some(idx) = app.selected_startup_idx
        && let Some(item) = items.get_mut(idx)
    {
        match app
            .actions
            .set_startup_enabled(&item.id, &item.location, enable)
        {
            Ok(()) => {
                item.enabled = enable;
                app.shared.toast(if enable {
                    i18n::tr(K::EnabledWord)
                } else {
                    i18n::tr(K::DisabledWord)
                });
            }
            Err(e) => app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()])),
        }
    }
}

/// Localized impact labels ("Keine", "Nicht gemessen", ...).
fn impact_label(lang: tm_core::i18n::Lang, impact: StartupImpact) -> &'static str {
    match impact {
        StartupImpact::None => i18n::tr_in(lang, K::ImpactNone),
        StartupImpact::Low => i18n::tr_in(lang, K::ImpactLow),
        StartupImpact::Medium => i18n::tr_in(lang, K::ImpactMedium),
        StartupImpact::High => i18n::tr_in(lang, K::ImpactHigh),
        StartupImpact::Unknown => i18n::tr_in(lang, K::ImpactUnknown),
    }
}

/// Percent-encode a query term well enough for a search URL.
fn urlencoding_lite(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Best-effort executable path out of a startup command line.
fn exe_from_command(cmd: &str) -> Option<String> {
    let cmd = cmd.trim();
    if let Some(rest) = cmd.strip_prefix('"')
        && let Some(exe) = rest.split('"').next()
    {
        return Some(exe.to_string());
    }
    // First whitespace-separated token that looks like a path.
    cmd.split_whitespace().next().map(str::to_string)
}

/// Properties dialog ("Eigenschaften").
pub fn properties_dialog(app: &mut TaskManApp, ctx: &egui::Context, _pal: &theme::Palette) {
    let mut open = true;
    egui::Window::new(i18n::tr(K::Properties))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let Some(item) = app.startup_props.clone() else {
                app.startup_props = None;
                return;
            };
            ui.set_min_width(420.0);
            egui::Grid::new("startup-props")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.weak(i18n::tr(K::ColName));
                    ui.label(&item.name);
                    ui.end_row();
                    ui.weak(i18n::tr(K::ColPublisher));
                    ui.label(item.publisher.clone().unwrap_or_default());
                    ui.end_row();
                    ui.weak(i18n::tr(K::PropCommand));
                    ui.label(&item.command);
                    ui.end_row();
                    ui.weak(i18n::tr(K::PropLocation));
                    ui.label(&item.location);
                    ui.end_row();
                    ui.weak(i18n::tr(K::ColStatus));
                    ui.label(if item.enabled {
                        i18n::tr(K::EnabledWord)
                    } else {
                        i18n::tr(K::DisabledWord)
                    });
                    ui.end_row();
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(i18n::tr(K::Close)).clicked() {
                        app.startup_props = None;
                    }
                });
            });
        });
    if !open {
        app.startup_props = None;
    }
}
