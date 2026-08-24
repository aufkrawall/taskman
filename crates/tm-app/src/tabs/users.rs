//! Users tab: TM-style table (Benutzer/Status/CPU/Arbeitsspeicher/Datenträger/
//! Netzwerk) with aggregate header, expandable per-user app groups and the
//! Trennen / Benutzerkonten verwalten commands.

use eframe::egui;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tm_core::format;
use tm_core::model::{ProcCategory, UserSession};

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::theme;
use crate::widgets::tablekit::{Aggregates, TmColumn, TmTable};

const COLS: [TmColumn; 6] = [
    TmColumn::text("user", "Benutzer", 0.0),
    TmColumn::text("status", "Status", 190.0),
    TmColumn::num("cpu", "CPU", 110.0),
    TmColumn::num("mem", "Arbeitsspeicher", 110.0),
    TmColumn::num("disk", "Datenträger", 110.0),
    TmColumn::num("net", "Netzwerk", 110.0),
];

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

    let caps = app.actions.capabilities();
    crate::app_ui::tab_header(
        app,
        ui,
        &pal,
        |app, ui| {
            let enabled = app.selected_user.is_some() && caps.user_disconnect;
            if crate::app_ui::cmd_button(ui, &pal, Icon::SlashCircle, "Trennen", enabled)
                && let Some(id) = app.selected_user {
                    match app
                        .actions
                        .control_user_session(id, tm_platform::actions::UserSessionAction::Disconnect)
                    {
                        Ok(()) => app.shared.toast("Sitzung getrennt"),
                        Err(e) => app.shared.toast(format!("Fehler: {e}")),
                    }
                }
            if crate::app_ui::cmd_button(ui, &pal, Icon::Users, "Benutzerkonten verwalten", true) {
                let _ = app.actions.run_new_task("ms-settings:otherusers", false);
            }
        },
        |_app, _ui| {},
    );

    let guard = app.shared.sessions_cache.clone();
    let cache = guard.lock();
    let Some((sessions, _)) = cache.as_ref() else { return };
    let Some(snap) = app.latest_snapshot() else { return };

    // Real user sessions only — drop session 0 / services sessions.
    let sessions: Vec<&UserSession> = sessions
        .iter()
        .filter(|s| {
            s.id != 0
                && !s.user.is_empty()
                && !s.user.to_lowercase().starts_with("session")
        })
        .collect();

    // Aggregate per-user stats from the snapshot.
    struct Agg {
        cpu: f64,
        mem: f64,
        disk: f64,
        net: f64,
        count: usize,
        /// Grouped app rows: shown name -> (cpu, mem, disk, net, count, exe).
        apps: HashMap<String, ([f64; 4], usize, Option<String>)>,
    }
    let mut aggs: HashMap<u32, Agg> = HashMap::new();
    for s in &sessions {
        let name = s.user.trim_start_matches('(').trim_end_matches(')');
        aggs.insert(s.id, Agg { cpu: 0.0, mem: 0.0, disk: 0.0, net: 0.0, count: 0, apps: HashMap::new() });
        let entry = aggs.get_mut(&s.id).expect("just inserted");
        for p in &snap.processes {
            if let Some(u) = &p.user
                && (u.eq_ignore_ascii_case(&s.user) || (!name.is_empty() && u.eq_ignore_ascii_case(name)))
            {
                entry.cpu += p.cpu_pct as f64;
                entry.mem += p.mem_bytes as f64;
                entry.disk += p.disk_read_bps + p.disk_write_bps;
                entry.net += p.net_recv_bps.unwrap_or(0.0) + p.net_sent_bps.unwrap_or(0.0);
                entry.count += 1;
                let app_name = p.shown_name().to_string();
                let slot = &mut entry.apps;
                let e = slot.entry(app_name).or_insert(([0.0; 4], 0, None));
                e.0[0] += p.cpu_pct as f64;
                e.0[1] += p.mem_bytes as f64;
                e.0[2] += p.disk_read_bps + p.disk_write_bps;
                e.0[3] += p.net_recv_bps.unwrap_or(0.0) + p.net_sent_bps.unwrap_or(0.0);
                e.1 += 1;
                if e.2.is_none() {
                    e.2 = p.exe_path.as_ref().map(|x| x.to_string_lossy().into_owned());
                }
            }
        }
    }

    let q = app.search.trim().to_lowercase();
    let table = TmTable::new(COLS.to_vec(), 340.0);
    let agg_hdr = Aggregates::from_snapshot(&snap);
    let aggs_hdr = agg_hdr.strings();
    {
        let total: f32 = snap.processes.iter().map(|p| p.cpu_pct).sum();
        eprintln!(
            "USERS-DBG: sys_cpu={:.1} sum_proc_cpu={:.1} mem_used={} mem_total={} used_pct={:.1} aggs={:?}",
            snap.cpu.utilization_pct,
            total,
            snap.memory.used_bytes,
            snap.memory.total_bytes,
            snap.memory.used_pct(),
            aggs_hdr,
        );
    }

    let avail = ui.available_width();
    table.header(ui, &pal, avail, None, Some(&aggs_hdr));

    egui::ScrollArea::vertical()
        .id_salt("users-table")
        .auto_shrink(false)
        .show(ui, |ui| {
            for s in &sessions {
                let Some(a) = aggs.get(&s.id) else { continue };
                let display = match &s.domain {
                    Some(d) if !d.is_empty() && d != "REDACTED-HOSTNAME" => {
                        format!("{d}\\{}", s.user)
                    }
                    _ => s.user.clone(),
                };
                if !q.is_empty() && !display.to_lowercase().contains(&q) && a.count == 0 {
                    continue;
                }

                let selected = app.selected_user == Some(s.id);
                let (rect, resp) = table.row(ui, &pal, avail, selected);

                // Chevron + person icon + "Name (n)".
                let toggled =
                    table.chevron(ui, rect, app.users_expanded_contains(s.id), true, &pal);
                if toggled {
                    app.toggle_user_expanded(s.id);
                }
                let icon_rect = egui::Rect::from_center_size(
                    egui::Pos2::new(rect.left() + 38.0, rect.center().y),
                    egui::vec2(18.0, 18.0),
                );
                crate::icons::draw_at(ui, icon_rect, Icon::Person, pal.accent);
                let name_rect = table.col_rect(0, avail, rect);
                ui.painter().text(
                    egui::Pos2::new(name_rect.left() + 58.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    format!("{} ({})", display, a.count),
                    egui::FontId::proportional(12.5),
                    pal.text,
                );

                // Status: session state (German); TM shows active users blank.
                let status = match s.state {
                    tm_core::model::UserSessionState::Active => "",
                    tm_core::model::UserSessionState::Disconnected => "Getrennt",
                    tm_core::model::UserSessionState::Idle => "Leerlauf",
                    tm_core::model::UserSessionState::Connected => "Verbunden",
                    _ => "",
                };
                table.text_cell(ui, avail, rect, 1, status, &pal, false);

                let texts = [
                    format::format_pct_de(a.cpu.min(100.0) as f32),
                    format::format_mb_de(a.mem as u64),
                    format::format_rate_de(a.disk),
                    format::format_mbit_de(a.net),
                ];
                let active_row = a.cpu > 0.0 || a.mem > 0.0 || a.disk > 0.0 || a.net > 0.0;
                let cells: Vec<(f32, String)> = vec![
                    ((a.cpu.min(100.0) / 100.0) as f32, texts[0].clone()),
                    (0.35, texts[1].clone()),
                    (0.0, texts[2].clone()),
                    (0.0, texts[3].clone()),
                ];
                table.heat_cells(ui, &pal, avail, rect, 2, &cells, active_row);

                if resp.clicked() {
                    app.selected_user = Some(s.id);
                }

                // Expanded: grouped app rows for this user.
                if app.users_expanded_contains(s.id) {
                    type AppRow<'a> = (&'a String, &'a ([f64; 4], usize, Option<String>));
                    let mut apps: Vec<AppRow<'_>> = a.apps.iter().collect();
                    apps.sort_by(|x, y| y.1 .0[1].partial_cmp(&x.1 .0[1]).unwrap_or(std::cmp::Ordering::Equal));
                    for (name, (vals, count, exe)) in apps {
                        let (r2, resp2) = table.row(ui, &pal, avail, false);
                        let tex = exe
                            .as_ref()
                            .and_then(|p| app.shared.icons.get(ui.ctx(), app.actions.as_ref(), p, 6));
                        table.icon_cell(ui, r2.translate(egui::vec2(22.0, 0.0)), tex.as_ref(), pal.accent);
                        let nr = table.col_rect(0, avail, r2);
                        let label = if *count > 1 {
                            format!("{name} ({count})")
                        } else {
                            name.clone()
                        };
                        ui.painter().text(
                            egui::Pos2::new(nr.left() + 58.0 + 22.0, r2.center().y),
                            egui::Align2::LEFT_CENTER,
                            label,
                            egui::FontId::proportional(12.5),
                            pal.text,
                        );
                        let texts = [
                            format::format_pct_de(vals[0] as f32),
                            format::format_mb_de(vals[1] as u64),
                            format::format_rate_de(vals[2]),
                            format::format_mbit_de(vals[3]),
                        ];
                        let cells = vec![
                            ((vals[0] / 100.0) as f32, texts[0].clone()),
                            (0.3, texts[1].clone()),
                            (0.0, texts[2].clone()),
                            (0.0, texts[3].clone()),
                        ];
                        table.heat_cells(ui, &pal, avail, r2, 2, &cells, vals.iter().any(|&v| v > 0.0));
                        if resp2.clicked() {
                            app.selected_user = Some(s.id);
                        }
                    }
                }
            }
            ui.add_space(12.0);
        });
}

// Small helpers to keep the borrow checker happy inside the loop above.
trait UsersExt {
    fn users_expanded_contains(&self, id: u32) -> bool;
    fn toggle_user_expanded(&mut self, id: u32);
}

impl UsersExt for TaskManApp {
    fn users_expanded_contains(&self, id: u32) -> bool {
        self.processes_state.expanded_users.contains(&id)
    }
    fn toggle_user_expanded(&mut self, id: u32) {
        if !self.processes_state.expanded_users.remove(&id) {
            self.processes_state.expanded_users.insert(id);
        }
    }
}

// Re-export ProcCategory so the classifier import stays honest.
#[allow(unused)]
fn _touch(_: ProcCategory) {}
