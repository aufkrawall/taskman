//! Users tab: TM-style table (Benutzer/Status/CPU/Arbeitsspeicher/Datenträger/
//! Netzwerk) with aggregate header, expandable per-user app groups and the
//! Trennen/Abmelden / Benutzerkonten verwalten commands.
//!
//! Aggregation runs in ONE pass keyed by session id (implement.md §18.3) —
//! no repeated username string comparisons and no ambiguity between
//! same-named accounts on different domains.

use eframe::egui;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::UserSession;

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::search;
use crate::theme;
use crate::widgets::tablekit::{self, Aggregates, HeatCell, TmColumn};

fn columns() -> Vec<TmColumn> {
    vec![
        // Audit P0.1: configured width — no elastic viewport fill.
        TmColumn::text("user", i18n::tr(K::TabUsers), 340.0),
        TmColumn::text("status", i18n::tr(K::ColStatus), 190.0),
        TmColumn::num("cpu", i18n::tr(K::ColCpu), 110.0),
        TmColumn::num("mem", i18n::tr(K::ColMemory), 110.0),
        TmColumn::num("disk", i18n::tr(K::ColDisk), 110.0),
        // Per-process network is not measurable; render "—" (§16.6).
        TmColumn::num("net", i18n::tr(K::ColNetwork), 110.0),
    ]
}

/// Per-session aggregates from the single process pass.
struct Agg {
    cpu: f64,
    mem: f64,
    disk: f64,
    count: usize,
    apps: HashMap<String, ([f64; 4], usize, Option<String>)>,
}

/// Flattened row model: user headers + their grouped app rows.
enum URow {
    /// Session header row.
    User(usize),
    /// Grouped app row under a user.
    App {
        name: String,
        exe: Option<String>,
        values: [f64; 4],
        count: usize,
    },
}

/// Per-column heat normalization across the WHOLE Users display model
/// (audit P0.2): one maximum per resource column over user headers and app
/// rows alike, computed before virtualization.
struct HeatMax {
    cpu: f64,
    mem: f64,
    disk: f64,
}

impl HeatMax {
    fn intensity(&self, v: &[f64; 4]) -> [f32; 4] {
        [
            tablekit::norm(v[0], self.cpu),
            tablekit::norm(v[1], self.mem),
            tablekit::norm(v[2], self.disk),
            0.0, // network unavailable on this platform
        ]
    }

    fn over(rows: impl Iterator<Item = [f64; 4]>) -> Self {
        let mut m = Self {
            cpu: 0.0,
            mem: 0.0,
            disk: 0.0,
        };
        for v in rows {
            m.cpu = m.cpu.max(v[0]);
            m.mem = m.mem.max(v[1]);
            m.disk = m.disk.max(v[2]);
        }
        m
    }
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    let ctx = ui.ctx().clone();

    // Lazy refresh of the session list in the background.
    {
        let stale = {
            let guard = tm_core::sync::lock(&app.shared.sessions_cache);
            match guard.as_ref() {
                Some((_, t)) => t.elapsed() > Duration::from_secs(5),
                None => true,
            }
        };
        if stale && app.shared.sessions_fetch.begin() {
            let cache = app.shared.sessions_cache.clone();
            let toasts = app.shared.toasts.clone();
            let done = app.shared.sessions_fetch.flag();
            let actions = app.actions.clone();
            let wake = {
                let ctx = ctx.clone();
                move || ctx.request_repaint()
            };
            let job = move || {
                let sessions = actions.list_user_sessions();
                if let Err(e) = &sessions {
                    crate::app::toast_from(
                        &toasts,
                        i18n::trf(K::SessionsUnavailable, &[&e.to_string()]),
                    );
                }
                *tm_core::sync::lock(&cache) = Some((sessions.unwrap_or_default(), Instant::now()));
                done.store(false, std::sync::atomic::Ordering::Relaxed);
                wake();
            };
            match &app.shared.executor {
                Some(executor) => executor.run_quiet(|| {}, job),
                None => job(),
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
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::SlashCircle,
                i18n::tr(K::DisconnectUser),
                enabled,
            ) && let Some(id) = app.selected_user
            {
                session_action(
                    app,
                    &ctx,
                    id,
                    tm_platform::actions::UserSessionAction::Disconnect,
                );
            }
            if crate::app_ui::cmd_button(ui, &pal, Icon::Person, i18n::tr(K::SignOut), enabled)
                && let Some(id) = app.selected_user
            {
                session_action(
                    app,
                    &ctx,
                    id,
                    tm_platform::actions::UserSessionAction::Logoff,
                );
            }
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::Users,
                i18n::tr(K::ManageUserAccounts),
                true,
            ) {
                // Non-blocking launch (§18.1) — never wait on the UI thread.
                let _ = app.actions.run_new_task("ms-settings:otherusers", false);
            }
        },
        |_app, ui| {
            if ui.button(i18n::tr(K::RefreshNow)).clicked() {
                _app.refresh_all();
                ui.close();
            }
        },
    );

    let sessions_arc = app.shared.sessions_cache.clone();
    let guard = tm_core::sync::lock(&sessions_arc);
    let Some((sessions_all, _)) = guard.as_ref() else {
        // Background fetch still in flight — centered placeholder instead
        // of a blank pane.
        ui.centered_and_justified(|ui| ui.label(i18n::tr(K::GatheringData)));
        return;
    };
    let Some(snap) = app.latest_snapshot() else {
        return;
    };

    // Real user sessions only — drop session 0 / services sessions.
    let sessions: Vec<&UserSession> = sessions_all
        .iter()
        .filter(|s| {
            s.id != 0 && !s.user.is_empty() && !s.user.to_lowercase().starts_with("session")
        })
        .collect();

    // ---- one-pass aggregation keyed by session id --------------------------
    let mut aggs: HashMap<u32, Agg> = HashMap::with_capacity(sessions.len());
    for s in &sessions {
        aggs.insert(
            s.id,
            Agg {
                cpu: 0.0,
                mem: 0.0,
                disk: 0.0,
                count: 0,
                apps: HashMap::new(),
            },
        );
    }
    for p in &snap.processes {
        let sid = p.session_id.or_else(|| {
            // Fallback when the sampler lacks the session id: match the first
            // session with this username (rare; ids are authoritative).
            sessions
                .iter()
                .find(|s| {
                    p.user
                        .as_deref()
                        .is_some_and(|u| u.eq_ignore_ascii_case(&s.user))
                })
                .map(|s| s.id)
        });
        let Some(sid) = sid else { continue };
        let Some(a) = aggs.get_mut(&sid) else {
            continue;
        };
        a.cpu += p.cpu_pct as f64;
        a.mem += p.mem_bytes as f64;
        a.disk += p.disk_read_bps + p.disk_write_bps;
        a.count += 1;
        let e = a
            .apps
            .entry(p.shown_name().to_string())
            .or_insert(([0.0; 4], 0, None));
        e.0[0] += p.cpu_pct as f64;
        e.0[1] += p.mem_bytes as f64;
        e.0[2] += p.disk_read_bps + p.disk_write_bps;
        if e.2.is_none() {
            e.2 = p
                .exe_path
                .as_ref()
                .map(|x| x.to_string_lossy().into_owned());
        }
        e.1 += 1;
    }

    // ---- flatten into display rows -----------------------------------------
    // Search (audit §5): a user row stays visible only when the query
    // matches the user's display name OR one of their aggregated apps. The
    // old condition ("name matches OR count > 0") kept every active user
    // visible regardless of the query — search effectively failed here.
    let q = search::Query::new(&app.search);
    let mut rows: Vec<URow> = Vec::new();
    for (i, s) in sessions.iter().enumerate() {
        let display = display_name(s, &snap.system.hostname);
        let a = &aggs[&s.id];
        if !q.is_empty()
            && !q.matches_any(
                std::iter::once(display.as_str()).chain(a.apps.keys().map(String::as_str)),
            )
        {
            continue;
        }
        rows.push(URow::User(i));
        if app.processes_state.expanded_users.contains(&s.id) {
            type AppEntry = ([f64; 4], usize, Option<String>);
            let mut apps: Vec<(&String, &AppEntry)> = a.apps.iter().collect();
            apps.sort_by(|x, y| {
                y.1.0[1]
                    .partial_cmp(&x.1.0[1])
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for (name, (vals, count, exe)) in apps {
                rows.push(URow::App {
                    name: name.clone(),
                    exe: exe.clone(),
                    values: *vals,
                    count: *count,
                });
            }
        }
    }

    // Per-column maxima over all DISPLAYED rows (headers + apps), before
    // virtualization (audit P0.2).
    let heat_max = HeatMax::over(rows.iter().map(|r| match r {
        URow::User(i) => {
            let s = sessions[*i];
            let a = &aggs[&s.id];
            [a.cpu, a.mem, a.disk, 0.0]
        }
        URow::App { values, .. } => *values,
    }));

    let agg_hdr = Aggregates::from_snapshot(&snap);
    let aggs_hdr = agg_hdr.strings();

    let mut table = app.make_table("users", columns());
    let avail = tablekit::table_avail(ui);
    tablekit::scrolled_rows(
        "users",
        ui,
        &pal,
        &mut table,
        avail,
        None,
        Some(&aggs_hdr),
        rows.len(),
        |ui, table, _avail, _content_w, range| {
            for ri in range {
                match rows.get(ri) {
                    Some(URow::User(i)) => {
                        let s = sessions[*i];
                        let a = &aggs[&s.id];
                        let display = display_name(s, &snap.system.hostname);
                        user_row_ui(
                            app,
                            ui,
                            &pal,
                            table,
                            &heat_max,
                            s,
                            a,
                            &display,
                            caps.user_disconnect,
                        );
                    }
                    Some(URow::App {
                        name,
                        exe,
                        values,
                        count,
                        ..
                    }) => {
                        app_row_ui(
                            app,
                            ui,
                            &pal,
                            table,
                            name,
                            exe.as_deref(),
                            values,
                            *count,
                            &heat_max,
                        );
                    }
                    None => {}
                }
            }
        },
    );
    app.persist_table(&table);
}

#[allow(clippy::too_many_arguments)]
fn user_row_ui(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    pal: &theme::Palette,
    table: &tablekit::TmTable,
    heat_max: &HeatMax,
    s: &UserSession,
    a: &Agg,
    display: &str,
    can_disconnect: bool,
) {
    let selected = app.selected_user == Some(s.id);
    let (rect, resp) = table.row(ui, pal, selected);

    let expanded = app.processes_state.expanded_users.contains(&s.id);
    let seed = egui::Id::new(("user-chev", s.id));
    let toggled = table.chevron(ui, rect, expanded, true, pal, seed);
    if toggled && !app.processes_state.expanded_users.remove(&s.id) {
        app.processes_state.expanded_users.insert(s.id);
    }
    let icon_rect = egui::Rect::from_center_size(
        egui::Pos2::new(rect.left() + 38.0, rect.center().y),
        egui::vec2(18.0, 18.0),
    );
    crate::icons::draw_at(ui, icon_rect, Icon::Person, pal.accent);
    let name_rect = table.col_rect(0, rect);
    ui.painter().text(
        egui::Pos2::new(name_rect.left() + 56.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{} ({})", display, a.count),
        egui::FontId::proportional(tablekit::FONT_ROW),
        pal.text,
    );

    // Status: session state (localized); TM shows active users blank.
    let status = match s.state {
        tm_core::model::UserSessionState::Active => "",
        tm_core::model::UserSessionState::Disconnected => i18n::tr(K::StDisconnected),
        tm_core::model::UserSessionState::Idle => i18n::tr(K::StIdle),
        tm_core::model::UserSessionState::Connected => i18n::tr(K::StConnected),
        _ => "",
    };
    table.text_cell(ui, rect, 1, status, pal, false);

    let texts = [
        format::format_pct_cell(a.cpu.min(100.0) as f32),
        format::format_mb(a.mem as u64),
        format::format_rate_mb(a.disk),
        "—".to_string(), // per-user network is not measurable (§16.6)
    ];
    let active_row = a.cpu > 0.0 || a.mem > 0.0 || a.disk > 0.0;
    // Intensities normalized per column across the whole model (audit P0.2).
    let cells: Vec<HeatCell> = heat_max
        .intensity(&[a.cpu, a.mem, a.disk, 0.0])
        .iter()
        .zip(texts.iter())
        .map(|(t, txt)| HeatCell::new(*t, txt.clone()))
        .collect();
    table.heat_cells(ui, pal, rect, 2, &cells, active_row);

    if resp.clicked() {
        app.selected_user = Some(s.id);
    }
    if can_disconnect {
        let ctx = ui.ctx().clone();
        resp.context_menu(|ui| {
            ui.set_min_width(150.0);
            if ui.button(i18n::tr(K::DisconnectUser)).clicked() {
                session_action(
                    app,
                    &ctx,
                    s.id,
                    tm_platform::actions::UserSessionAction::Disconnect,
                );
                ui.close();
            }
            if ui.button(i18n::tr(K::SignOut)).clicked() {
                session_action(
                    app,
                    &ctx,
                    s.id,
                    tm_platform::actions::UserSessionAction::Logoff,
                );
                ui.close();
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn app_row_ui(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    pal: &theme::Palette,
    table: &tablekit::TmTable,
    name: &str,
    exe: Option<&str>,
    vals: &[f64; 4],
    count: usize,
    heat_max: &HeatMax,
) {
    let (rect, _resp) = table.row(ui, pal, false);
    let tex = exe.and_then(|p| app.shared.icons.get(ui.ctx(), &app.actions, p, 6));
    table.icon_cell(
        ui,
        rect.translate(egui::vec2(22.0, 0.0)),
        tex.as_ref(),
        pal.accent,
    );
    let nr = table.col_rect(0, rect);
    let label = if count > 1 {
        format!("{name} ({count})")
    } else {
        name.to_string()
    };
    ui.painter().text(
        egui::Pos2::new(nr.left() + 56.0 + 22.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(tablekit::FONT_ROW),
        pal.text,
    );
    let texts = [
        format::format_pct_cell(vals[0] as f32),
        format::format_mb(vals[1] as u64),
        format::format_rate_mb(vals[2]),
        "—".to_string(),
    ];
    let cells: Vec<HeatCell> = heat_max
        .intensity(vals)
        .iter()
        .zip(texts.iter())
        .map(|(t, txt)| HeatCell::new(*t, txt.clone()))
        .collect();
    table.heat_cells(ui, pal, rect, 2, &cells, vals.iter().any(|&v| v > 0.0));
}

fn display_name(s: &UserSession, hostname: &str) -> String {
    match &s.domain {
        // Local accounts: TM shows the bare user name; only prefix foreign
        // domains (no hardcoded host names).
        Some(d)
            if !d.is_empty()
                && !d.eq_ignore_ascii_case(hostname)
                && !s.user.ends_with(&format!("@{d}")) =>
        {
            format!("{d}\\{}", s.user)
        }
        _ => s.user.clone(),
    }
}

fn session_action(
    app: &mut TaskManApp,
    ctx: &egui::Context,
    id: u32,
    action: tm_platform::actions::UserSessionAction,
) {
    let actions = app.actions.clone();
    app.run_action(
        ctx,
        move || match action {
            tm_platform::actions::UserSessionAction::Disconnect => {
                i18n::tr(K::SessionDisconnected).to_string()
            }
            tm_platform::actions::UserSessionAction::Logoff => {
                i18n::tr(K::UserSignedOut).to_string()
            }
        },
        move || actions.control_user_session(id, action),
    );
}
