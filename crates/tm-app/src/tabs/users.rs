//! Users tab: sessions with aggregated resource usage and (on Windows)
//! disconnect/logoff actions.

use eframe::egui;
use std::time::{Duration, Instant};
use tm_core::model::ProcCategory;

use crate::app::TaskManApp;
use crate::theme;

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);

    // Lazy refresh of the session list.
    {
        let cache = app.shared.sessions_cache.clone();
        let mut guard = cache.lock();
        let stale = match guard.as_ref() {
            Some((_, t)) => t.elapsed() > Duration::from_secs(5),
            None => true,
        };
        if stale {
            match app.actions.list_user_sessions() {
                Ok(sessions) => *guard = Some((sessions, Instant::now())),
                Err(e) => {
                    app.shared.toast(format!("Sitzungen nicht verfügbar: {e}"));
                    *guard = Some((vec![], Instant::now()));
                }
            }
        }
    }

    ui.heading("Benutzer");
    ui.add_space(2.0);
    ui.separator();

    let guard = app.shared.sessions_cache.clone();
    let cache = guard.lock();
    let Some((sessions, _)) = cache.as_ref() else {
        return;
    };

    let snap = app.latest_snapshot();

    egui::ScrollArea::vertical()
        .id_salt("users")
        .show(ui, |ui| {
            for s in sessions {
                // Aggregate this user's processes from the snapshot.
                let mut cpu = 0.0f32;
                let mut mem = 0u64;
                let mut count = 0usize;
                if let Some(ref snap) = snap {
                    let name = s.user.trim_start_matches('(').trim_end_matches(')');
                    for p in &snap.processes {
                        if let Some(ref u) = p.user
                            && (u.eq_ignore_ascii_case(&s.user)
                                || (!name.is_empty()
                                    && !name.starts_with("session")
                                    && u.eq_ignore_ascii_case(name)))
                        {
                            cpu += p.cpu_pct;
                            mem += p.mem_bytes;
                            count += 1;
                        }
                    }
                }

                egui::Frame::group(ui.style())
                    .fill(pal.card_bg)
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            crate::icons::draw_at(
                                ui,
                                egui::Rect::from_center_size(
                                    ui.cursor().left_center() + egui::vec2(12.0, -8.0),
                                    egui::vec2(24.0, 24.0),
                                ),
                                crate::icons::Icon::Users,
                                pal.accent,
                            );
                            ui.vertical(|ui| {
                                let display = match &s.domain {
                                    Some(d) => format!("{d}\\{}", s.user),
                                    None => s.user.clone(),
                                };
                                ui.label(egui::RichText::new(display).strong().size(14.0));
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} · {} Prozesse",
                                        s.state.label(),
                                        count
                                    ))
                                    .size(11.5)
                                    .color(pal.text_dim),
                                );
                            });

                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    stat(
                                        ui,
                                        &pal,
                                        "Arbeitsspeicher",
                                        &tm_core::format::format_bytes(mem),
                                    );
                                    stat(ui, &pal, "CPU", &format!("{cpu:.0} %"));

                                    #[cfg(target_os = "windows")]
                                    {
                                        use tm_platform::actions::UserSessionAction;
                                        if ui.button("Abmelden").clicked() {
                                            match app.actions.control_user_session(
                                                s.id,
                                                UserSessionAction::Logoff,
                                            ) {
                                                Ok(()) => app.shared.toast("Abgemeldet"),
                                                Err(e) => app.shared.toast(format!("Fehler: {e}")),
                                            }
                                        }
                                        if ui.button("Trennen").clicked() {
                                            match app.actions.control_user_session(
                                                s.id,
                                                UserSessionAction::Disconnect,
                                            ) {
                                                Ok(()) => app.shared.toast("Sitzung getrennt"),
                                                Err(e) => app.shared.toast(format!("Fehler: {e}")),
                                            }
                                        }
                                    }
                                    let _ = &mut cpu;
                                },
                            );
                        });
                    });
                ui.add_space(6.0);
            }
        });
}

fn stat(ui: &mut egui::Ui, pal: &theme::Palette, label: &str, value: &str) {
    ui.vertical(|ui| {
        ui.label(egui::RichText::new(label).size(11.0).color(pal.text_dim));
        ui.label(egui::RichText::new(value).size(13.5));
    });
}

// Silence unused-import warning for ProcCategory on some platforms.
#[allow(unused)]
fn _touch(_: ProcCategory) {}
