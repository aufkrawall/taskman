//! Process Properties → Network: the TCP connections/listeners and UDP
//! endpoints of one process, refreshed while the page is on screen.
//!
//! The connection tables carry only a PID, no creation time. The identity is
//! therefore checked against the newest snapshot both before a query starts
//! and before its result is shown; a PID recycled inside that sub-second
//! window is the remaining (accepted) gap.

use eframe::egui;
use std::cmp::Ordering;
use std::time::{Duration, Instant};
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::ProcessEntry;
use tm_platform::actions::{ConnectionProtocol, ProcessConnection, TcpState};

use crate::app::{ProcessIdentity, TaskManApp};
use crate::search;
use crate::tabs::inspector::{self, Poll};
use crate::theme;
use crate::widgets::tablekit::{self, TmColumn};

const REFRESH_EVERY: Duration = Duration::from_secs(2);
const TABLE_ID: &str = "proc_network";

type Key = (
    ConnectionProtocol,
    std::net::SocketAddr,
    Option<std::net::SocketAddr>,
);

pub struct State {
    poll: Poll<Vec<ProcessConnection>>,
    rows: Option<Result<Vec<ProcessConnection>, String>>,
    selected: Option<Key>,
    sort: usize,
    ascending: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            poll: Poll::default(),
            rows: None,
            selected: None,
            sort: 0,
            ascending: true,
        }
    }
}

fn key_of(connection: &ProcessConnection) -> Key {
    (connection.protocol, connection.local, connection.remote)
}

pub fn tick(state: &mut State, app: &TaskManApp, ctx: &egui::Context, identity: &ProcessIdentity) {
    if let Some(result) = state.poll.take() {
        // Stale by the time it arrived: the PID may now name another process.
        state.rows = Some(if app.identity_is_live(identity) {
            result
        } else {
            Err(i18n::tr(K::ProcessExited).to_string())
        });
    }
    if !app.actions.capabilities().process_connections || !app.identity_is_live(identity) {
        return;
    }
    if state.poll.due(Instant::now(), Some(REFRESH_EVERY)) {
        let actions = app.actions.clone();
        let pid = identity.pid;
        state.poll.start(ctx, "tm-connections", move || {
            actions
                .list_process_connections(pid)
                .map_err(|error| error.to_string())
        });
    }
    ctx.request_repaint_after(REFRESH_EVERY);
}

fn protocol_label(protocol: ConnectionProtocol) -> &'static str {
    match protocol {
        ConnectionProtocol::Tcp => "TCP",
        ConnectionProtocol::Tcp6 => "TCPv6",
        ConnectionProtocol::Udp => "UDP",
        ConnectionProtocol::Udp6 => "UDPv6",
    }
}

/// netstat's state vocabulary: protocol terms, not prose, so not localized.
fn state_label(state: Option<TcpState>) -> String {
    let Some(state) = state else {
        return String::new();
    };
    match state {
        TcpState::Closed => "Closed",
        TcpState::Listen => "Listen",
        TcpState::SynSent => "SYN sent",
        TcpState::SynReceived => "SYN received",
        TcpState::Established => "Established",
        TcpState::FinWait1 => "FIN wait 1",
        TcpState::FinWait2 => "FIN wait 2",
        TcpState::CloseWait => "Close wait",
        TcpState::Closing => "Closing",
        TcpState::LastAck => "Last ACK",
        TcpState::TimeWait => "Time wait",
        TcpState::DeleteTcb => "Delete TCB",
        TcpState::Other(raw) => return raw.to_string(),
    }
    .to_string()
}

fn compare(a: &ProcessConnection, b: &ProcessConnection, column: usize) -> Ordering {
    let primary = match column {
        0 => protocol_label(a.protocol).cmp(protocol_label(b.protocol)),
        1 => a.local.ip().cmp(&b.local.ip()),
        2 => a.local.port().cmp(&b.local.port()),
        3 => a.remote.map(|r| r.ip()).cmp(&b.remote.map(|r| r.ip())),
        4 => a.remote.map(|r| r.port()).cmp(&b.remote.map(|r| r.port())),
        _ => state_label(a.state).cmp(&state_label(b.state)),
    };
    primary
        .then_with(|| a.local.port().cmp(&b.local.port()))
        .then_with(|| a.remote.cmp(&b.remote))
}

pub fn show(
    app: &mut TaskManApp,
    ui: &mut egui::Ui,
    state: &mut State,
    process: &ProcessEntry,
    pal: &theme::Palette,
) {
    let ctx = ui.ctx().clone();
    // The live per-process rates come from the sampler, next to the
    // endpoints that carry them.
    let rate = |value: Option<f64>| value.map_or_else(|| "—".to_string(), format::format_rate);
    ui.horizontal(|ui| {
        ui.weak(format!("{}:", i18n::tr(K::PropNetworkReceive)));
        ui.label(rate(process.net_recv_bps));
        ui.add_space(16.0);
        ui.weak(format!("{}:", i18n::tr(K::PropNetworkSend)));
        ui.label(rate(process.net_sent_bps));
    })
    .response
    .on_hover_text(i18n::tr(K::ConnectionsPidTip));
    ui.add_space(4.0);

    let rows = match &state.rows {
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(i18n::tr(K::GatheringData));
            });
            return;
        }
        Some(Err(error)) => {
            ui.colored_label(pal.heat_high, error);
            return;
        }
        Some(Ok(rows)) => rows,
    };
    let mut rows: Vec<&ProcessConnection> = rows.iter().collect();
    rows.sort_by(|a, b| tablekit::directed(compare(a, b, state.sort), state.ascending));

    let dialog_open = app.modal_open();
    search::set_active_content(&ctx, TABLE_ID);
    if let Some(nav) = search::list_nav(&ctx, dialog_open) {
        let current = state
            .selected
            .and_then(|key| rows.iter().position(|row| key_of(row) == key));
        let page_rows = tablekit::page_rows(&ctx, TABLE_ID, tablekit::ROW_H_DENSE).unwrap_or(10);
        if let Some(index) = search::moved_index(rows.len(), current, nav, page_rows) {
            state.selected = rows.get(index).map(|row| key_of(row));
        }
    }

    let cols = vec![
        TmColumn::text("protocol", i18n::tr(K::ColProtocol), 80.0),
        TmColumn::text("local", i18n::tr(K::ColLocalAddress), 220.0),
        TmColumn::num("local_port", i18n::tr(K::ColPort), 70.0),
        TmColumn::text("remote", i18n::tr(K::ColRemoteAddress), 220.0),
        TmColumn::num("remote_port", i18n::tr(K::ColPort), 70.0),
        TmColumn::text("state", i18n::tr(K::ColConnState), 120.0),
    ];
    let mut table = app
        .make_table(TABLE_ID, cols)
        .with_row_height(tablekit::ROW_H_DENSE);
    let avail = tablekit::table_avail(ui);
    let mut clicked_key = None;
    let clicked = tablekit::scrolled_rows(
        TABLE_ID,
        ui,
        pal,
        &mut table,
        avail,
        Some((state.sort, state.ascending)),
        None,
        rows.len(),
        Some(i18n::tr(K::NoConnections)),
        None,
        None,
        |ui, table, _avail, _content_width, range| {
            for index in range {
                let Some(row) = rows.get(index) else {
                    continue;
                };
                let key = key_of(row);
                let (rect, response) = table.row(ui, pal, state.selected == Some(key), key);
                let local_ip = row.local.ip().to_string();
                table.describe_row(ui, &response, &local_ip, index);
                table.text_cell(ui, rect, 0, protocol_label(row.protocol), pal, false);
                table.text_cell(ui, rect, 1, &local_ip, pal, false);
                inspector::num_cell(
                    ui,
                    table,
                    rect,
                    2,
                    &row.local.port().to_string(),
                    pal,
                    false,
                );
                match row.remote {
                    Some(remote) => {
                        table.text_cell(ui, rect, 3, &remote.ip().to_string(), pal, false);
                        inspector::num_cell(
                            ui,
                            table,
                            rect,
                            4,
                            &remote.port().to_string(),
                            pal,
                            false,
                        );
                    }
                    None => {
                        table.text_cell(ui, rect, 3, "", pal, false);
                    }
                }
                table.text_cell(ui, rect, 5, &state_label(row.state), pal, false);
                if response.clicked() || response.secondary_clicked() {
                    clicked_key = Some(key);
                }
            }
        },
    );
    if clicked_key.is_some() {
        state.selected = clicked_key;
    }
    if let Some(column) = clicked {
        if state.sort == column {
            state.ascending = !state.ascending;
        } else {
            state.sort = column;
            state.ascending = true;
        }
    }
    app.persist_table(&table);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(local: &str, remote: Option<&str>, state: Option<TcpState>) -> ProcessConnection {
        ProcessConnection {
            protocol: if state.is_some() {
                ConnectionProtocol::Tcp
            } else {
                ConnectionProtocol::Udp
            },
            local: local.parse().unwrap(),
            remote: remote.map(|r| r.parse().unwrap()),
            state,
        }
    }

    #[test]
    fn ports_sort_numerically_not_textually() {
        let a = connection("127.0.0.1:9000", None, Some(TcpState::Listen));
        let b = connection("127.0.0.1:10000", None, Some(TcpState::Listen));
        assert_eq!(compare(&a, &b, 2), Ordering::Less);
    }

    #[test]
    fn udp_endpoints_have_no_state_text() {
        assert_eq!(state_label(None), "");
        assert_eq!(state_label(Some(TcpState::TimeWait)), "Time wait");
    }
}
