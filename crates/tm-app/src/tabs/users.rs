//! Users tab: TM-style table (Benutzer/Status/CPU/Arbeitsspeicher/Datenträger/
//! Netzwerk) with aggregate header, expandable per-user app groups and the
//! Trennen/Abmelden / Benutzerkonten verwalten commands.
//!
//! Aggregation runs in ONE pass keyed by session id (implement.md §18.3) —
//! no repeated username string comparisons and no ambiguity between
//! same-named accounts on different domains.

use eframe::egui;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::UserSession;

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::search;
use crate::theme;
use crate::widgets::menu;
use crate::widgets::tablekit::{self, Aggregates, HeatCell, TmColumn};
use tm_platform::actions::UserSessionAction;

fn columns() -> Vec<TmColumn> {
    vec![
        TmColumn::text("user", i18n::tr(K::TabUsers), 340.0),
        TmColumn::text("status", i18n::tr(K::ColStatus), 190.0),
        TmColumn::num("cpu", i18n::tr(K::ColCpu), 110.0),
        TmColumn::num("mem", i18n::tr(K::ColMemory), 110.0),
        TmColumn::num("disk", i18n::tr(K::ColDisk), 110.0),
        TmColumn::num("net", i18n::tr(K::ColNetwork), 110.0),
    ]
}

struct Agg {
    cpu: f64,
    mem: f64,
    disk: f64,
    net: f64,
    /// At least one process in this session reported network bytes. Without
    /// one, the sum is not "no traffic" but "not measured", and the cell has
    /// to say so instead of printing a zero nobody measured.
    net_known: bool,
    count: usize,
    apps: HashMap<String, AppAgg>,
}

/// One app name's rollup inside a session.
struct AppAgg {
    values: [f64; VALUES],
    count: usize,
    exe: Option<String>,
    net_known: bool,
}

/// CPU, memory, disk, network - the numeric columns, in table order.
const VALUES: usize = 4;

/// Index of the network value inside a row's values.
const NET: usize = 3;

enum URow {
    User(usize),
    App {
        session: u32,
        name: String,
        exe: Option<String>,
        values: [f64; VALUES],
        count: usize,
        net_known: bool,
    },
}

/// Fold one process into its session's rollup and into that session's entry
/// for the app it belongs to.
fn accumulate(a: &mut Agg, p: &tm_core::model::ProcessEntry) {
    let net = p.net_recv_bps.unwrap_or(0.0) + p.net_sent_bps.unwrap_or(0.0);
    let net_known = p.net_recv_bps.is_some() || p.net_sent_bps.is_some();
    a.cpu += p.cpu_pct as f64;
    a.mem += p.mem_bytes as f64;
    a.disk += p.disk_read_bps + p.disk_write_bps;
    a.net += net;
    a.net_known |= net_known;
    a.count += 1;
    let e = a.apps.entry(p.shown_name().to_string()).or_insert(AppAgg {
        values: [0.0; VALUES],
        count: 0,
        exe: None,
        net_known: false,
    });
    e.values[0] += p.cpu_pct as f64;
    e.values[1] += p.mem_bytes as f64;
    e.values[2] += p.disk_read_bps + p.disk_write_bps;
    e.values[NET] += net;
    e.net_known |= net_known;
    if e.exe.is_none() {
        e.exe = p
            .exe_path
            .as_ref()
            .map(|x| x.to_string_lossy().into_owned());
    }
    e.count += 1;
}

struct HeatMax {
    cpu: f64,
    mem: f64,
    disk: f64,
    net: f64,
}

impl HeatMax {
    fn intensity(&self, v: &[f64; VALUES], net_known: bool) -> [f32; VALUES] {
        [
            tablekit::norm(v[0], self.cpu),
            tablekit::norm(v[1], self.mem),
            tablekit::norm(v[2], self.disk),
            // An unmeasured cell must not colour the heat band, nor pull the
            // column maximum below.
            if net_known {
                tablekit::norm(v[NET], self.net)
            } else {
                0.0
            },
        ]
    }

    fn over(rows: impl Iterator<Item = ([f64; VALUES], bool)>) -> Self {
        let mut m = Self {
            cpu: 0.0,
            mem: 0.0,
            disk: 0.0,
            net: 0.0,
        };
        for (v, net_known) in rows {
            m.cpu = m.cpu.max(v[0]);
            m.mem = m.mem.max(v[1]);
            m.disk = m.disk.max(v[2]);
            if net_known {
                m.net = m.net.max(v[NET]);
            }
        }
        m
    }
}

/// The numeric cells of one row, with unmeasured network rendered as "—".
fn value_texts(values: &[f64; VALUES], net_known: bool) -> [String; VALUES] {
    [
        format::format_pct_cell(values[0].min(100.0) as f32),
        format::format_mb(values[1] as u64),
        format::format_rate_mb(values[2]),
        if net_known {
            format::format_process_net_rate(values[NET])
        } else {
            "—".to_string()
        },
    ]
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    let ctx = ui.ctx().clone();

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
                Some(executor) => {
                    if !executor.run_quiet(|| {}, job) {
                        app.shared.sessions_fetch.end();
                        app.shared.toast(i18n::tr(K::ActionQueueFull));
                    }
                }
                None => {
                    drop(job);
                    app.shared.sessions_fetch.end();
                    app.shared.toast(i18n::tr(K::ActionFailed));
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
        ui.centered_and_justified(|ui| ui.label(i18n::tr(K::GatheringData)));
        return;
    };
    let Some(snap) = app.latest_snapshot() else {
        return;
    };

    let sessions: Vec<&UserSession> = sessions_all
        .iter()
        .filter(|s| {
            s.id != 0 && !s.user.is_empty() && !s.user.to_lowercase().starts_with("session")
        })
        .collect();

    let mut aggs: HashMap<u32, Agg> = HashMap::with_capacity(sessions.len());
    for s in &sessions {
        aggs.insert(
            s.id,
            Agg {
                cpu: 0.0,
                mem: 0.0,
                disk: 0.0,
                net: 0.0,
                net_known: false,
                count: 0,
                apps: HashMap::new(),
            },
        );
    }
    for p in &snap.processes {
        let sid = p.session_id.or_else(|| {
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
        accumulate(a, p);
    }

    let q = search::Query::new(&app.search);
    let mut rows: Vec<URow> = Vec::new();
    let mut visible_sessions = sessions
        .iter()
        .enumerate()
        .filter_map(|(index, session)| {
            let display = display_name(session, &snap.system.hostname);
            let agg = &aggs[&session.id];
            (q.is_empty()
                || q.matches_any(
                    std::iter::once(display.as_str()).chain(agg.apps.keys().map(String::as_str)),
                ))
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let sort = app.users_sort;
    visible_sessions.sort_by(|left, right| {
        let a = sessions[*left];
        let b = sessions[*right];
        compare_users(
            a,
            &aggs[&a.id],
            b,
            &aggs[&b.id],
            &snap.system.hostname,
            sort,
        )
    });
    for i in visible_sessions {
        let s = sessions[i];
        let a = &aggs[&s.id];
        rows.push(URow::User(i));
        if app.processes_state.expanded_users.contains(&s.id) {
            let mut apps: Vec<(&String, &AppAgg)> = a.apps.iter().collect();
            apps.sort_by(|left, right| {
                compare_user_apps(left.0, &left.1.values, right.0, &right.1.values, sort)
            });
            for (name, entry) in apps {
                rows.push(URow::App {
                    session: s.id,
                    name: name.clone(),
                    exe: entry.exe.clone(),
                    values: entry.values,
                    count: entry.count,
                    net_known: entry.net_known,
                });
            }
        }
    }

    let heat_max = HeatMax::over(rows.iter().map(|r| match r {
        URow::User(i) => {
            let s = sessions[*i];
            let a = &aggs[&s.id];
            ([a.cpu, a.mem, a.disk, a.net], a.net_known)
        }
        URow::App {
            values, net_known, ..
        } => (*values, *net_known),
    }));

    let agg_hdr = Aggregates::from_snapshot(&snap);
    let aggs_hdr = agg_hdr.strings();

    let mut table = app.make_table("users", columns());
    prepare_auto_fit_widths(
        ui, app, &mut table, &rows, &sessions, &aggs, &snap, &aggs_hdr,
    );
    let avail = tablekit::table_avail(ui);
    let clicked = tablekit::scrolled_rows(
        "users",
        ui,
        &pal,
        &mut table,
        avail,
        Some((app.users_sort.column, app.users_sort.ascending)),
        Some(&aggs_hdr),
        rows.len(),
        None,
        None,
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
                        session,
                        name,
                        exe,
                        values,
                        count,
                        net_known,
                    }) => {
                        app_row_ui(
                            app,
                            ui,
                            &pal,
                            table,
                            *session,
                            name,
                            exe.as_deref(),
                            values,
                            *count,
                            *net_known,
                            &heat_max,
                        );
                    }
                    None => {}
                }
            }
        },
    );
    if let Some(column) = clicked {
        app.users_sort.clicked(column, column >= 2);
        let ids = ["user", "status", "cpu", "mem", "disk", "net"];
        app.persist_sort("users", ids[column], app.users_sort.ascending);
    }
    app.persist_table(&table);
}

fn directed(order: Ordering, ascending: bool) -> Ordering {
    if ascending { order } else { order.reverse() }
}

fn compare_users(
    a: &UserSession,
    aa: &Agg,
    b: &UserSession,
    ba: &Agg,
    hostname: &str,
    sort: tablekit::SortState,
) -> Ordering {
    let a_name = display_name(a, hostname).to_ascii_lowercase();
    let b_name = display_name(b, hostname).to_ascii_lowercase();
    let primary = match sort.column {
        0 => a_name.cmp(&b_name),
        1 => session_state_rank(a.state).cmp(&session_state_rank(b.state)),
        2 => aa.cpu.partial_cmp(&ba.cpu).unwrap_or(Ordering::Equal),
        3 => aa.mem.partial_cmp(&ba.mem).unwrap_or(Ordering::Equal),
        4 => aa.disk.partial_cmp(&ba.disk).unwrap_or(Ordering::Equal),
        // An unmeasured network sorts below every measured one instead of
        // tying with a session that genuinely sent nothing.
        _ => net_key(aa)
            .partial_cmp(&net_key(ba))
            .unwrap_or(Ordering::Equal),
    };
    directed(primary, sort.ascending).then_with(|| a_name.cmp(&b_name))
}

/// Sort key for the network column: `None` where nothing measured it.
fn net_key(a: &Agg) -> Option<f64> {
    a.net_known.then_some(a.net)
}

fn compare_user_apps(
    a_name: &str,
    a_values: &[f64; VALUES],
    b_name: &str,
    b_values: &[f64; VALUES],
    sort: tablekit::SortState,
) -> Ordering {
    let names = || {
        a_name
            .to_ascii_lowercase()
            .cmp(&b_name.to_ascii_lowercase())
    };
    let primary = match sort.column {
        2..=5 => a_values[sort.column - 2]
            .partial_cmp(&b_values[sort.column - 2])
            .unwrap_or(Ordering::Equal),
        _ => names(),
    };
    directed(primary, sort.ascending).then_with(names)
}

fn session_state_rank(state: tm_core::model::UserSessionState) -> u8 {
    use tm_core::model::UserSessionState as State;
    match state {
        State::Active => 0,
        State::Connected => 1,
        State::ConnectQuery => 2,
        State::Shadowing => 3,
        State::Disconnected => 4,
        State::Idle => 5,
        State::Listen => 6,
        State::Reset => 7,
        State::Down => 8,
        State::Init => 9,
        State::Unknown => 10,
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_auto_fit_widths(
    ui: &egui::Ui,
    app: &TaskManApp,
    table: &mut tablekit::TmTable,
    rows: &[URow],
    sessions: &[&UserSession],
    aggs: &HashMap<u32, Agg>,
    snap: &tm_core::model::Snapshot,
    agg_hdr: &[String],
) {
    let mut fit: Vec<f32> = table
        .cols
        .iter()
        .map(|c| tablekit::text_width(ui, c.label, tablekit::FONT_HDR_LABEL) + 28.0)
        .collect();
    for row in rows {
        let (name, status, values, net_known, name_extra) = match row {
            URow::User(i) => {
                let s = sessions[*i];
                let a = &aggs[&s.id];
                (
                    format!("{} ({})", display_name(s, &snap.system.hostname), a.count),
                    session_status_label(s),
                    [a.cpu, a.mem, a.disk, a.net],
                    a.net_known,
                    66.0,
                )
            }
            URow::App {
                name,
                values,
                count,
                net_known,
                ..
            } => (
                if *count > 1 {
                    format!("{name} ({count})")
                } else {
                    name.clone()
                },
                "",
                *values,
                *net_known,
                88.0,
            ),
        };
        fit[0] = fit[0].max(tablekit::text_width(ui, &name, tablekit::FONT_ROW) + name_extra);
        fit[1] = fit[1].max(tablekit::text_width(ui, status, tablekit::FONT_ROW) + 22.0);
        let texts = value_texts(&values, net_known);
        for (i, text) in texts.iter().enumerate() {
            fit[i + 2] = fit[i + 2].max(tablekit::text_width(ui, text, tablekit::FONT_ROW) + 22.0);
        }
    }
    // Zip against the table's OWN numeric columns: this list is shared with
    // the Processes page, which has more of them.
    for (col, agg) in table
        .numeric_indices()
        .zip(agg_hdr.iter())
        .collect::<Vec<_>>()
    {
        fit[col] = fit[col].max(tablekit::text_width(ui, agg, tablekit::FONT_AGG) + 36.0);
    }
    for (i, width) in fit.into_iter().enumerate() {
        table.set_auto_fit_width(i, width.ceil());
    }
    let _ = app;
}

fn session_status_label(s: &UserSession) -> &'static str {
    match s.state {
        tm_core::model::UserSessionState::Active => "",
        tm_core::model::UserSessionState::Disconnected => i18n::tr(K::StDisconnected),
        tm_core::model::UserSessionState::Idle => i18n::tr(K::StIdle),
        tm_core::model::UserSessionState::Connected => i18n::tr(K::StConnected),
        _ => "",
    }
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
    let (rect, resp) = table.row(ui, pal, selected, ("user", s.id));

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
    ui.painter_at(name_rect).text(
        egui::Pos2::new(name_rect.left() + 56.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{} ({})", display, a.count),
        egui::FontId::proportional(tablekit::FONT_ROW),
        pal.text,
    );

    table.text_cell(ui, rect, 1, session_status_label(s), pal, false);

    let values = [a.cpu, a.mem, a.disk, a.net];
    let texts = value_texts(&values, a.net_known);
    let cells: Vec<HeatCell> = heat_max
        .intensity(&values, a.net_known)
        .iter()
        .zip(texts.iter())
        .map(|(t, txt)| HeatCell::new(*t, txt.clone()))
        .collect();
    table.heat_cells(ui, pal, rect, 2, &cells);
    let net_tip = (!a.net_known)
        .then(|| unavailable_network_tip(ui, table, rect))
        .flatten();

    if resp.clicked() {
        app.selected_user = Some(s.id);
    }
    // on_hover_text consumes the response (builder style), so it has to come
    // before the context menu is attached to it.
    let resp = match net_tip {
        Some(tip) => resp.on_hover_text(tip),
        None => resp,
    };
    if can_disconnect {
        let ctx = ui.ctx().clone();
        let keyboard_open = menu::keyboard_menu_requested(&ctx) && app.selected_user == Some(s.id);
        menu::context_menu_kb(&resp, keyboard_open, |ui| {
            ui.set_min_width(150.0);
            if menu::item(ui, i18n::tr(K::DisconnectUser)).clicked() {
                session_action(
                    app,
                    &ctx,
                    s.id,
                    tm_platform::actions::UserSessionAction::Disconnect,
                );
                ui.close();
            }
            if menu::item(ui, i18n::tr(K::SignOut)).clicked() {
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
    session_id: u32,
    name: &str,
    exe: Option<&str>,
    vals: &[f64; VALUES],
    count: usize,
    net_known: bool,
    heat_max: &HeatMax,
) {
    // Same app name can appear under two users, so the key pairs the row
    // with its session.
    let (rect, resp) = table.row(ui, pal, false, ("app", session_id, name));
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
    ui.painter_at(nr).text(
        egui::Pos2::new(nr.left() + 56.0 + 22.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(tablekit::FONT_ROW),
        pal.text,
    );
    let texts = value_texts(vals, net_known);
    let cells: Vec<HeatCell> = heat_max
        .intensity(vals, net_known)
        .iter()
        .zip(texts.iter())
        .map(|(t, txt)| HeatCell::new(*t, txt.clone()))
        .collect();
    table.heat_cells(ui, pal, rect, 2, &cells);
    if !net_known
        && let Some(tip) = unavailable_network_tip(ui, table, rect)
    {
        resp.on_hover_text(tip);
    }
}

/// Explain the "—" in the Network column the way the Processes page does:
/// per-process bytes come from an ETW session, and without it the honest
/// answer is "unknown".
fn unavailable_network_tip(
    ui: &egui::Ui,
    table: &tablekit::TmTable,
    rect: egui::Rect,
) -> Option<&'static str> {
    let cell = table.col_rect(2 + NET, rect);
    let pointer = ui.ctx().pointer_latest_pos()?;
    cell.contains(pointer)
        .then_some(i18n::tr(K::NetPerProcessUnavailable))
}

fn display_name(s: &UserSession, hostname: &str) -> String {
    match &s.domain {
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
    // High blast radius: signing out a session destroys its unsaved work.
    // Native Task Manager also confirms this step — never a one-click wipe
    // (security audit, GUI high-blast-radius safety). Disconnect is
    // non-destructive and stays immediate.
    if action == UserSessionAction::Logoff {
        let hostname = app
            .latest_snapshot()
            .map(|s| s.system.hostname.clone())
            .unwrap_or_default();
        let name = tm_core::sync::lock(&app.shared.sessions_cache)
            .as_ref()
            .and_then(|(list, _)| {
                list.iter()
                    .find(|s| s.id == id)
                    .map(|s| display_name(s, &hostname))
            })
            .unwrap_or_else(|| format!("(session {id})"));
        app.pending_session_logoff = Some((id, name));
        return;
    }
    dispatch_session_action(app, ctx, id, action);
}

fn dispatch_session_action(
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

/// Confirmation dialog for the pending sign-out (`session_action` parks
/// Logoff here instead of dispatching it right away).
pub fn session_logoff_dialog(app: &mut TaskManApp, ctx: &egui::Context, _pal: &theme::Palette) {
    use egui::{Align2, Window};
    let Some((id, name)) = app.pending_session_logoff.clone() else {
        return;
    };
    let mut open = true;
    Window::new(i18n::tr(K::SignOut))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(i18n::trf(K::SignOutConfirm, &[&name]));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(i18n::tr(K::Cancel)).clicked() {
                    app.pending_session_logoff = None;
                }
                if ui.button(i18n::tr(K::Yes)).clicked() {
                    app.pending_session_logoff = None;
                    dispatch_session_action(app, ctx, id, UserSessionAction::Logoff);
                }
            });
        });
    if !open {
        app.pending_session_logoff = None;
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::model::ProcessEntry;

    fn empty_agg() -> Agg {
        Agg {
            cpu: 0.0,
            mem: 0.0,
            disk: 0.0,
            net: 0.0,
            net_known: false,
            count: 0,
            apps: HashMap::new(),
        }
    }

    fn with_net(name: &str, recv: Option<f64>, sent: Option<f64>) -> ProcessEntry {
        let mut p = ProcessEntry::new(1, name);
        p.net_recv_bps = recv;
        p.net_sent_bps = sent;
        p
    }

    /// The session row sums what the per-process trace measured, instead of
    /// the flat "—" it printed before there was a network rollup at all.
    #[test]
    fn a_session_sums_the_network_rates_of_its_processes() {
        let mut a = empty_agg();
        accumulate(&mut a, &with_net("a.exe", Some(1024.0), Some(512.0)));
        accumulate(&mut a, &with_net("b.exe", Some(2048.0), None));
        assert!(a.net_known);
        assert_eq!(a.net, 3584.0);
        // Not an exact string: the decimal separator follows the locale.
        let text = value_texts(&[0.0, 0.0, 0.0, a.net], a.net_known)[NET].clone();
        assert!(text.starts_with("3") && text.ends_with(" KB/s"), "{text}");
    }

    /// Core invariant: telemetry nobody measured renders as unknown, never as
    /// a zero, and it must not colour the heat band or set its maximum.
    #[test]
    fn an_unmeasured_session_network_stays_unknown() {
        let mut a = empty_agg();
        accumulate(&mut a, &with_net("a.exe", None, None));
        assert!(!a.net_known);
        assert_eq!(value_texts(&[0.0, 0.0, 0.0, a.net], a.net_known)[NET], "\u{2014}");

        let heat = HeatMax::over([([0.0, 0.0, 0.0, 9_000.0], false)].into_iter());
        assert_eq!(heat.net, 0.0);
        assert_eq!(heat.intensity(&[0.0, 0.0, 0.0, 9_000.0], false)[NET], 0.0);
    }

    /// A process that measured zero bytes is a measurement: it prints a rate,
    /// and only the absence of any reading falls back to unknown.
    #[test]
    fn a_measured_zero_is_not_unknown() {
        let mut a = empty_agg();
        accumulate(&mut a, &with_net("a.exe", Some(0.0), Some(0.0)));
        assert!(a.net_known);
        assert_eq!(
            value_texts(&[0.0, 0.0, 0.0, a.net], a.net_known)[NET],
            format::format_process_net_rate(0.0)
        );
    }

    /// Unknown sorts below every measured value rather than tying with a
    /// session that genuinely sent nothing.
    #[test]
    fn unknown_network_sorts_below_a_measured_zero() {
        let mut measured = empty_agg();
        accumulate(&mut measured, &with_net("a.exe", Some(0.0), None));
        let mut unknown = empty_agg();
        accumulate(&mut unknown, &with_net("b.exe", None, None));
        assert!(net_key(&unknown) < net_key(&measured));
    }
}
