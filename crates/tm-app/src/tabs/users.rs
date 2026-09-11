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
use tm_core::i18n::{self, K};
use tm_core::model::UserSession;

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::search;
use crate::tabs::value_columns::{self, FIXED_COLS, VALUE_COLS};
use crate::theme;
use crate::widgets::menu;
use crate::widgets::tablekit::{self, Aggregates, HeatCell, TmColumn};
use tm_platform::actions::UserSessionAction;

/// Stable ids of every column in LOGICAL order, for sort persistence.
pub fn column_ids() -> Vec<&'static str> {
    value_columns::column_ids(["user", "status"])
}

fn columns(value_order: &[usize]) -> Vec<TmColumn> {
    value_columns::columns(
        vec![
            TmColumn::text("user", i18n::tr(K::TabUsers), 340.0),
            TmColumn::text("status", i18n::tr(K::ColStatus), 190.0),
        ],
        value_order,
    )
}

/// CPU, memory and the disk byte rates come straight off every process, so a
/// session with no processes at all honestly totals zero. The last three come
/// from traces that may not be running; until one of them delivers, their sum
/// is "not measured", not "nothing happened".
const ALWAYS_KNOWN: [bool; VALUE_COLS] = [true, true, true, false, false, false];

/// What a set of processes adds up to, in the shared LOGICAL column order.
#[derive(Clone)]
struct Roll {
    values: [f64; VALUE_COLS],
    /// Per column: did ANY process in this rollup produce a reading?
    known: [bool; VALUE_COLS],
    count: usize,
}

impl Roll {
    fn new() -> Self {
        Self {
            values: [0.0; VALUE_COLS],
            known: ALWAYS_KNOWN,
            count: 0,
        }
    }

    /// Fold one process in. Optional telemetry contributes its value AND the
    /// fact that it was measured; a process the trace never saw contributes
    /// neither, so it cannot drag a real rate down to a fake zero.
    fn add(&mut self, p: &tm_core::model::ProcessEntry) {
        self.values[0] += p.cpu_pct as f64;
        self.values[1] += p.mem_bytes as f64;
        self.values[2] += p.disk_read_bps + p.disk_write_bps;
        if p.net_recv_bps.is_some() || p.net_sent_bps.is_some() {
            self.values[value_columns::NET] +=
                p.net_recv_bps.unwrap_or(0.0) + p.net_sent_bps.unwrap_or(0.0);
            self.known[value_columns::NET] = true;
        }
        if let Some(pct) = p.disk_active_pct {
            self.values[value_columns::DISK_ACT] += f64::from(pct);
            self.known[value_columns::DISK_ACT] = true;
        }
        if let Some(pct) = p.gpu_util_pct {
            self.values[value_columns::GPU] += f64::from(pct);
            self.known[value_columns::GPU] = true;
        }
        self.count += 1;
    }

    /// Sort key for one column: `None` where nothing measured it, which orders
    /// below every measured value instead of tying with a real zero.
    fn key(&self, li: usize) -> Option<f64> {
        self.known[li].then_some(self.values[li])
    }

    /// The row's cells, in LOGICAL order.
    fn texts(&self) -> [String; VALUE_COLS] {
        std::array::from_fn(|li| value_columns::value_text(li, self.values[li], self.known[li]))
    }
}

struct Agg {
    roll: Roll,
    apps: HashMap<String, AppAgg>,
}

/// One app name's rollup inside a session.
struct AppAgg {
    roll: Roll,
    exe: Option<String>,
}

enum URow {
    User(usize),
    App {
        session: u32,
        name: String,
        exe: Option<String>,
        roll: Roll,
    },
}

/// Fold one process into its session's rollup and into that session's entry
/// for the app it belongs to.
fn accumulate(a: &mut Agg, p: &tm_core::model::ProcessEntry) {
    a.roll.add(p);
    let e = a.apps.entry(p.shown_name().to_string()).or_insert(AppAgg {
        roll: Roll::new(),
        exe: None,
    });
    e.roll.add(p);
    if e.exe.is_none() {
        e.exe = p
            .exe_path
            .as_ref()
            .map(|x| x.to_string_lossy().into_owned());
    }
}

/// Per-column maximum, for the heat band. Unmeasured cells are left out: they
/// must neither colour the band nor set the scale the measured ones are drawn
/// against.
struct HeatMax {
    max: [f64; VALUE_COLS],
}

impl HeatMax {
    fn intensity(&self, roll: &Roll) -> [f32; VALUE_COLS] {
        std::array::from_fn(|li| {
            if roll.known[li] {
                tablekit::norm(roll.values[li], self.max[li])
            } else {
                0.0
            }
        })
    }

    fn over<'a>(rolls: impl Iterator<Item = &'a Roll>) -> Self {
        let mut m = Self {
            max: [0.0; VALUE_COLS],
        };
        for roll in rolls {
            for li in 0..VALUE_COLS {
                if roll.known[li] {
                    m.max[li] = m.max[li].max(roll.values[li]);
                }
            }
        }
        m
    }
}

/// Paint one row's numeric block in the user's display order, and explain any
/// cell that reads "—" while the cursor is on it.
fn heat_row(
    ui: &egui::Ui,
    pal: &theme::Palette,
    table: &tablekit::TmTable,
    rect: egui::Rect,
    order: &[usize],
    roll: &Roll,
    heat_max: &HeatMax,
) -> Option<&'static str> {
    let texts = value_columns::in_display_order(order, &roll.texts());
    let heat = value_columns::in_display_order(order, &heat_max.intensity(roll));
    let cells: Vec<HeatCell> = heat
        .iter()
        .zip(texts)
        .map(|(t, text)| HeatCell::new(*t, text))
        .collect();
    table.heat_cells(ui, pal, rect, FIXED_COLS, &cells);

    let pointer = ui.ctx().pointer_latest_pos()?;
    order.iter().enumerate().find_map(|(slot, &li)| {
        (!roll.known[li] && table.col_rect(FIXED_COLS + slot, rect).contains(pointer))
            .then(|| value_columns::unavailable_tip(li))
            .flatten()
    })
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
                roll: Roll::new(),
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

    let order = app.users_value_order.clone();
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
                compare_user_apps(left.0, &left.1.roll, right.0, &right.1.roll, sort)
            });
            for (name, entry) in apps {
                rows.push(URow::App {
                    session: s.id,
                    name: name.clone(),
                    exe: entry.exe.clone(),
                    roll: entry.roll.clone(),
                });
            }
        }
    }

    let heat_max = HeatMax::over(rows.iter().map(|r| match r {
        URow::User(i) => &aggs[&sessions[*i].id].roll,
        URow::App { roll, .. } => roll,
    }));

    // Header totals are machine totals, produced in logical order and shown
    // in the user's display order.
    let logical_hdr = Aggregates::from_snapshot(&snap).strings();
    let aggs_hdr = value_columns::in_display_order(&order, &logical_hdr);

    let mut table = app
        .make_table("users", columns(&order))
        // Only the numeric block is draggable: `heat_cells` paints the blue
        // band as one contiguous span, and the first cell owns the chevron.
        .reorderable(FIXED_COLS..FIXED_COLS + VALUE_COLS);
    prepare_auto_fit_widths(
        ui, app, &mut table, &rows, &sessions, &aggs, &snap, &aggs_hdr, &order,
    );
    let avail = tablekit::table_avail(ui);
    let clicked = tablekit::scrolled_rows(
        "users",
        ui,
        &pal,
        &mut table,
        avail,
        Some((
            value_columns::display_col(&order, app.users_sort.column),
            app.users_sort.ascending,
        )),
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
                            &order,
                            caps.user_disconnect,
                        );
                    }
                    Some(URow::App {
                        session,
                        name,
                        exe,
                        roll,
                    }) => {
                        app_row_ui(
                            app,
                            ui,
                            &pal,
                            table,
                            *session,
                            name,
                            exe.as_deref(),
                            roll,
                            &order,
                            &heat_max,
                        );
                    }
                    None => {}
                }
            }
        },
    );
    if let Some(column) = clicked {
        // The sort is held (and persisted) in LOGICAL terms: the display index
        // it was clicked at means nothing once the columns are dragged around.
        let logical = value_columns::logical_col(&order, column);
        app.users_sort.clicked(logical, logical >= FIXED_COLS);
        app.persist_sort("users", column_ids()[logical], app.users_sort.ascending);
    }
    if let Some((from, to)) = table.take_reorder()
        && value_columns::move_column(&mut app.users_value_order, from, to)
    {
        // Widths are stored per column id, so they follow their column; only
        // the order itself has to be written out.
        let ids = value_columns::saved_ids(&app.users_value_order);
        app.persist_column_order("users", ids, &value_columns::default_ids());
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
        li => compare_values(&aa.roll, &ba.roll, li),
    };
    directed(primary, sort.ascending).then_with(|| a_name.cmp(&b_name))
}

/// Order two rollups by one LOGICAL value column. An unmeasured cell sorts
/// below every measured one instead of tying with a real zero.
fn compare_values(a: &Roll, b: &Roll, logical: usize) -> Ordering {
    let li = logical - FIXED_COLS;
    a.key(li).partial_cmp(&b.key(li)).unwrap_or(Ordering::Equal)
}

fn compare_user_apps(
    a_name: &str,
    a_roll: &Roll,
    b_name: &str,
    b_roll: &Roll,
    sort: tablekit::SortState,
) -> Ordering {
    let names = || {
        a_name
            .to_ascii_lowercase()
            .cmp(&b_name.to_ascii_lowercase())
    };
    let primary = match sort.column {
        li if li >= FIXED_COLS => compare_values(a_roll, b_roll, li),
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
    order: &[usize],
) {
    let mut fit: Vec<f32> = table
        .cols
        .iter()
        .map(|c| tablekit::text_width(ui, c.label, tablekit::FONT_HDR_LABEL) + 28.0)
        .collect();
    for row in rows {
        let (name, status, roll, name_extra) = match row {
            URow::User(i) => {
                let s = sessions[*i];
                let a = &aggs[&s.id];
                (
                    format!(
                        "{} ({})",
                        display_name(s, &snap.system.hostname),
                        a.roll.count
                    ),
                    session_status_label(s),
                    &a.roll,
                    66.0,
                )
            }
            URow::App { name, roll, .. } => (
                if roll.count > 1 {
                    format!("{name} ({})", roll.count)
                } else {
                    name.clone()
                },
                "",
                roll,
                88.0,
            ),
        };
        fit[0] = fit[0].max(tablekit::text_width(ui, &name, tablekit::FONT_ROW) + name_extra);
        fit[1] = fit[1].max(tablekit::text_width(ui, status, tablekit::FONT_ROW) + 22.0);
        let texts = value_columns::in_display_order(order, &roll.texts());
        for (slot, text) in texts.iter().enumerate() {
            fit[FIXED_COLS + slot] = fit[FIXED_COLS + slot]
                .max(tablekit::text_width(ui, text, tablekit::FONT_ROW) + 22.0);
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
    order: &[usize],
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
        format!("{} ({})", display, a.roll.count),
        egui::FontId::proportional(tablekit::FONT_ROW),
        pal.text,
    );

    table.text_cell(ui, rect, 1, session_status_label(s), pal, false);

    let unknown_tip = heat_row(ui, pal, table, rect, order, &a.roll, heat_max);

    if resp.clicked() {
        app.selected_user = Some(s.id);
    }
    // on_hover_text consumes the response (builder style), so it has to come
    // before the context menu is attached to it.
    let resp = match unknown_tip {
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
    roll: &Roll,
    order: &[usize],
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
    let label = if roll.count > 1 {
        format!("{name} ({})", roll.count)
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
    if let Some(tip) = heat_row(ui, pal, table, rect, order, roll, heat_max) {
        resp.on_hover_text(tip);
    }
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
    use value_columns::{DISK_ACT, GPU, NET};

    fn empty_agg() -> Agg {
        Agg {
            roll: Roll::new(),
            apps: HashMap::new(),
        }
    }

    fn with_net(name: &str, recv: Option<f64>, sent: Option<f64>) -> ProcessEntry {
        let mut p = ProcessEntry::new(1, name);
        p.net_recv_bps = recv;
        p.net_sent_bps = sent;
        p
    }

    /// The session row sums what the per-process traces measured, instead of
    /// the flat "—" it printed before there was a rollup at all.
    #[test]
    fn a_session_sums_the_optional_telemetry_of_its_processes() {
        let mut a = empty_agg();
        let mut first = with_net("a.exe", Some(1024.0), Some(512.0));
        first.disk_active_pct = Some(4.0);
        first.gpu_util_pct = Some(10.0);
        accumulate(&mut a, &first);
        accumulate(&mut a, &with_net("b.exe", Some(2048.0), None));

        assert_eq!(a.roll.count, 2);
        assert!(a.roll.known[NET] && a.roll.known[DISK_ACT] && a.roll.known[GPU]);
        assert_eq!(a.roll.values[NET], 3584.0);
        assert_eq!(a.roll.values[DISK_ACT], 4.0);
        assert_eq!(a.roll.values[GPU], 10.0);
        // Not an exact string: the decimal separator follows the locale.
        let text = a.roll.texts()[NET].clone();
        assert!(text.starts_with('3') && text.ends_with(" KB/s"), "{text}");
    }

    /// Core invariant: telemetry nobody measured renders as unknown, never as
    /// a zero, and it must not colour the heat band or set its maximum.
    #[test]
    fn unmeasured_columns_stay_unknown() {
        let mut a = empty_agg();
        accumulate(&mut a, &with_net("a.exe", None, None));
        for li in [NET, DISK_ACT, GPU] {
            assert!(!a.roll.known[li]);
            assert_eq!(a.roll.texts()[li], "—");
        }
        // ...while the columns every process reports stay real numbers.
        assert!(a.roll.known[0] && a.roll.known[1] && a.roll.known[2]);

        let mut loud = Roll::new();
        loud.values[NET] = 9_000.0;
        let heat = HeatMax::over([&loud].into_iter());
        assert_eq!(heat.max[NET], 0.0, "an unknown cell must not set the scale");
        assert_eq!(heat.intensity(&loud)[NET], 0.0);
    }

    /// A process that measured zero is a measurement: it prints a value, and
    /// only the absence of any reading falls back to unknown.
    #[test]
    fn a_measured_zero_is_not_unknown() {
        let mut a = empty_agg();
        let mut p = with_net("a.exe", Some(0.0), Some(0.0));
        p.gpu_util_pct = Some(0.0);
        accumulate(&mut a, &p);
        assert!(a.roll.known[NET] && a.roll.known[GPU]);
        assert_ne!(a.roll.texts()[NET], "—");
        assert_ne!(a.roll.texts()[GPU], "—");
    }

    /// Unknown sorts below every measured value rather than tying with a
    /// session that genuinely sent nothing.
    #[test]
    fn unknown_sorts_below_a_measured_zero() {
        let mut measured = empty_agg();
        accumulate(&mut measured, &with_net("a.exe", Some(0.0), None));
        let mut unknown = empty_agg();
        accumulate(&mut unknown, &with_net("b.exe", None, None));
        assert!(unknown.roll.key(NET) < measured.roll.key(NET));
        assert_eq!(
            compare_values(&unknown.roll, &measured.roll, FIXED_COLS + NET),
            Ordering::Less
        );
    }

    /// The page shows the same six measurements as the Processes page, and
    /// the sort persists by id in LOGICAL order.
    #[test]
    fn the_page_mirrors_the_shared_column_catalogue() {
        let ids = column_ids();
        assert_eq!(ids.len(), FIXED_COLS + VALUE_COLS);
        assert_eq!(&ids[..2], &["user", "status"]);
        assert_eq!(ids[FIXED_COLS + GPU], "gpu");

        // Dragging GPU to the front changes where it is drawn, not what the
        // sort means.
        let mut order = value_columns::default_order();
        assert!(value_columns::move_column(
            &mut order,
            FIXED_COLS + GPU,
            FIXED_COLS
        ));
        let cols = columns(&order);
        assert_eq!(cols[FIXED_COLS].id, "gpu");
        assert_eq!(
            value_columns::logical_col(&order, FIXED_COLS),
            FIXED_COLS + GPU
        );
    }
}
