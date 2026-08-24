//! UI parts of the root app: navigation rail, status bar, dialogs, toasts.

use eframe::egui::{self, Color32};
use tm_core::settings::ThemeMode;

use crate::app::TaskManApp;
use crate::theme;

pub fn apply_theme(ctx: &egui::Context, mode: ThemeMode) {
    ctx.set_theme(match mode {
        ThemeMode::System => egui::ThemePreference::System,
        ThemeMode::Light => egui::ThemePreference::Light,
        ThemeMode::Dark => egui::ThemePreference::Dark,
    });
}

impl TaskManApp {
    pub fn sidebar(&mut self, ui_root: &mut egui::Ui, pal: &theme::Palette) {
        egui::Panel::left(egui::Id::new("nav"))
            .resizable(false)
            .min_size(190.0)
            .max_size(190.0)
            .frame(
                egui::Frame::NONE
                    .fill(pal.sidebar_bg)
                    .inner_margin(egui::Margin::symmetric(8, 8)),
            )
            .show(ui_root, |ui| {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("Task Manager")
                        .size(15.0)
                        .strong()
                        .color(pal.text),
                );
                ui.add_space(10.0);

                for tab in crate::app::Tab::ALL {
                    let selected = self.tab == tab;
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 34.0),
                        egui::Sense::click(),
                    );
                    if response.clicked() {
                        self.tab = tab;
                    }
                    if selected {
                        ui.painter()
                            .rect_filled(rect, 5.0, pal.accent.gamma_multiply(0.22));
                        let r = egui::Rect::from_min_size(
                            rect.left_top() + egui::vec2(1.0, 7.0),
                            egui::vec2(3.0, rect.height() - 14.0),
                        );
                        ui.painter().rect_filled(r, 2.0, pal.accent);
                    } else if response.hovered() {
                        ui.painter().rect_filled(rect, 5.0, pal.card_bg_hover.gamma_multiply(0.8));
                    }
                    let icon_rect = egui::Rect::from_center_size(
                        rect.left_center() + egui::vec2(22.0, 0.0),
                        egui::vec2(20.0, 20.0),
                    );
                    crate::icons::draw_at(ui, icon_rect, tab.icon(), pal.text);
                    ui.painter().text(
                        rect.left_center() + egui::vec2(42.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        tab.label(),
                        egui::FontId::proportional(13.5),
                        if selected { pal.text } else { pal.text_dim },
                    );
                }

                // Bottom actions.
                ui.add_space(ui.available_height() - 84.0);

                if nav_button(ui, pal, crate::icons::Icon::Plus, "Neuen Task ausführen") {
                    self.run_dialog_open = true;
                }
                ui.add_space(4.0);
                if nav_button(ui, pal, crate::icons::Icon::Settings, "Einstellungen") {
                    self.show_settings = true;
                }
            });
    }

    pub fn status_bar(&mut self, ui_root: &mut egui::Ui, pal: &theme::Palette) {
        egui::Panel::bottom(egui::Id::new("status"))
            .resizable(false)
            .min_size(26.0)
            .max_size(26.0)
            .frame(
                egui::Frame::NONE
                    .fill(pal.panel_bg)
                    .inner_margin(egui::Margin::symmetric(10, 3)),
            )
            .show(ui_root, |ui| {
                ui.horizontal(|ui| {
                    if let Some(snap) = self.engine.latest() {
                        chip(ui, pal, &format!("Prozesse: {}", snap.processes.len()));
                        ui.separator();
                        chip(ui, pal, &format!("CPU: {:.0} %", snap.cpu.utilization_pct));
                        ui.separator();
                        chip(
                            ui,
                            pal,
                            &format!(
                                "Arbeitsspeicher: {} ({:.0} %)",
                                tm_core::format::format_bytes(snap.memory.used_bytes),
                                snap.memory.used_pct()
                            ),
                        );
                    } else if self.engine.state() == tm_core::EngineState::Paused {
                        chip(ui, pal, "Pausiert");
                    } else {
                        chip(ui, pal, "Sammle Daten…");
                    }
                });
            });
    }

    pub fn settings_dialog(&mut self, ctx: &egui::Context, _pal: &theme::Palette) {
        let mut open = true;
        egui::Window::new("Einstellungen")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(360.0);

                ui.heading("Design");
                ui.horizontal(|ui| {
                    for (mode, label) in [
                        (ThemeMode::System, "System"),
                        (ThemeMode::Light, "Hell"),
                        (ThemeMode::Dark, "Dunkel"),
                    ] {
                        if ui
                            .selectable_label(self.shared.settings.theme == mode, label)
                            .clicked()
                        {
                            self.shared.settings.theme = mode;
                            apply_theme(ctx, mode);
                        }
                    }
                });

                ui.add_space(10.0);
                ui.heading("Aktualisierungsgeschwindigkeit");
                ui.horizontal_wrapped(|ui| {
                    for speed in [
                        tm_core::settings::UpdateSpeed::High,
                        tm_core::settings::UpdateSpeed::Normal,
                        tm_core::settings::UpdateSpeed::Low,
                        tm_core::settings::UpdateSpeed::Paused,
                    ] {
                        if ui
                            .selectable_label(self.shared.settings.update_speed == speed, speed.label())
                            .clicked()
                        {
                            self.shared.settings.update_speed = speed;
                            match speed {
                                tm_core::settings::UpdateSpeed::Paused => self.engine.pause(),
                                _ => {
                                    self.engine.resume();
                                    self.engine.set_interval(speed.interval());
                                }
                            }
                        }
                    }
                });

                ui.add_space(10.0);
                let mut on_top = self.shared.settings.always_on_top;
                if ui.checkbox(&mut on_top, "Immer im Vordergrund").changed() {
                    self.shared.settings.always_on_top = on_top;
                    ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                        if on_top {
                            egui::WindowLevel::AlwaysOnTop
                        } else {
                            egui::WindowLevel::Normal
                        },
                    ));
                }

                ui.add_space(10.0);
                ui.label("Diagrammfenster:");
                ui.horizontal(|ui| {
                    for secs in [30u32, 60, 120] {
                        if ui
                            .selectable_label(self.shared.settings.graph_seconds == secs, format!("{secs} s"))
                            .clicked()
                        {
                            self.shared.settings.graph_seconds = secs;
                        }
                    }
                });

                ui.add_space(14.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Zurücksetzen").clicked() {
                        let defaults = Settings::default();
                        apply_theme(ctx, defaults.theme);
                        self.engine.resume();
                        self.engine.set_interval(defaults.update_speed.interval());
                        self.shared.settings = defaults;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Schließen").clicked() {
                            self.show_settings = false;
                            self.shared.settings.save();
                        }
                    });
                });
            });
        if !open {
            self.show_settings = false;
        }
    }

    pub fn run_task_dialog(&mut self, ctx: &egui::Context, pal: &theme::Palette) {
        let mut open = true;
        egui::Window::new("Neuen Task ausführen")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
            .show(ctx, |ui| {
                use crate::theme as t;
                let pal = t::palette(ui);
                ui.set_width(420.0);
                ui.label("Name des Programms, Ordners oder Dokuments:");
                ui.add_space(4.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.run_dialog_text)
                        .hint_text("z. B. notepad")
                        .desired_width(f32::INFINITY),
                );
                resp.request_focus();
                ui.add_space(4.0);
                ui.checkbox(&mut self.run_elevated, "Mit Administratorrechten ausführen");
                let _ = pal;
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Abbrechen").clicked() {
                        self.run_dialog_open = false;
                    }
                    let enter =
                        resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.button("OK").clicked() || enter) && !self.run_dialog_text.trim().is_empty()
                    {
                        let cmdline = self.run_dialog_text.trim().to_string();
                        match self.actions.run_new_task(&cmdline, self.run_elevated) {
                            Ok(()) => {
                                self.shared.toast(format!("Gestartet: {cmdline}"));
                                self.run_dialog_open = false;
                            }
                            Err(e) => self.shared.toast(format!("Fehler: {e}")),
                        }
                    }
                });
            });
        if !open {
            self.run_dialog_open = false;
        }
    }

    pub fn draw_toasts(&self, ctx: &egui::Context) {
        let mut toasts = self.shared.toasts.lock();
        toasts.retain(|(_, born)| born.elapsed() < std::time::Duration::from_secs(4));
        let mut y_offset = 0.0f32;
        for (msg, born) in toasts.iter() {
            let age = born.elapsed().as_secs_f32();
            let alpha = ((4.0 - age) * 255.0).clamp(90.0, 255.0) as u8;
            let id = egui::Id::new(format!("toast-{}", born.elapsed().as_nanos()));
            egui::Area::new(id)
                .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -12.0 - y_offset])
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::window(ui.style())
                        .fill(Color32::from_black_alpha(alpha.min(220)))
                        .stroke(egui::Stroke::new(1.0, theme::LIGHT.stroke))
                        .inner_margin(egui::Margin::same(8))
                        .show(ui, |ui| {
                            ui.set_max_width(380.0);
                            ui.label(
                                egui::RichText::new(msg)
                                    .size(12.5)
                                    .color(Color32::from_white_alpha(alpha)),
                            );
                        });
                });
            y_offset += 46.0;
        }
    }
}

fn nav_button(ui: &mut egui::Ui, pal: &theme::Palette, icon: crate::icons::Icon, label: &str) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 5.0, pal.card_bg_hover.gamma_multiply(0.8));
    }
    let icon_rect = egui::Rect::from_center_size(
        rect.left_center() + egui::vec2(22.0, 0.0),
        egui::vec2(18.0, 18.0),
    );
    crate::icons::draw_at(ui, icon_rect, icon, pal.text_dim);
    ui.painter().text(
        rect.left_center() + egui::vec2(42.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(12.5),
        pal.text_dim,
    );
    response.clicked()
}

fn chip(ui: &mut egui::Ui, pal: &theme::Palette, text: &str) {
    ui.label(egui::RichText::new(text).size(11.5).color(pal.text_dim));
}

// Settings re-import used inside dialog.
use tm_core::settings::Settings;
