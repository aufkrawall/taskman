//! Details tab: dense flat process table with the Win11 TM default columns —
//! Name, PID, Status, Benutzername, CPU, Arbeitsspeicher, Plattform,
//! Heraufgestuft, UAC-Virtualisierung, GPU-Modul.

use eframe::egui;
use tm_core::format;
use tm_core::i18n::{self, K};
use tm_core::model::{PriorityClass, ProcStatus, ProcessEntry};

use crate::app::TaskManApp;
use crate::icons::Icon;
use crate::theme;
use crate::widgets::tablekit::{TmColumn};

fn columns() -> Vec<TmColumn> {
    vec![
        TmColumn::text("name", i18n::tr(K::ColName), 0.0),
        TmColumn::text("pid", i18n::tr(K::ColPid), 90.0),
        TmColumn::text("status", i18n::tr(K::ColStatus), 150.0),
        TmColumn::text("user", i18n::tr(K::ColUsername), 120.0),
        TmColumn::text("cpu", i18n::tr(K::ColCpu), 64.0),
        TmColumn::num("mem", i18n::tr(K::ColMemory), 130.0),
        TmColumn::text("platform", i18n::tr(K::ColPlatform), 90.0),
        TmColumn::text("elevated", i18n::tr(K::ColElevated), 110.0),
        TmColumn::text("uac", i18n::tr(K::ColUac), 160.0),
        TmColumn::text("gpu", i18n::tr(K::ColGpuEngine), 110.0),
    ]
}

#[derive(Default)]
pub struct State {
    pub sort_col: usize,
    pub ascending: bool,
    pub filter: String,
    cache: Option<Cache>,
}

struct Cache {
    key: (u64, String, usize, bool, u8),
    rows: Vec<Row>,
}

pub struct Row {
    pub pid: u32,
    pub name: String,
    pub icon_path: Option<String>,
    pub pid_s: String,
    pub status: &'static str,
    pub user: String,
    pub cpu_s: String,
    pub mem_s: String,
    pub platform: &'static str,
    pub elevated: &'static str,
    pub uac: &'static str,
}

pub fn show(app: &mut TaskManApp, ui: &mut egui::Ui) {
    let pal = theme::palette(ui);
    let Some(snap) = app.latest_snapshot() else {
        ui.centered_and_justified(|ui| ui.label(i18n::tr(K::GatheringData)));
        return;
    };

    crate::app_ui::tab_header(
        app,
        ui,
        &pal,
        |app: &mut TaskManApp, ui| {
            if crate::app_ui::cmd_button(
                ui,
                &pal,
                Icon::Close,
                i18n::tr(K::EndTask),
                app.selected_pid.is_some(),
            ) {
                app.end_selected();
            }
        },
        |_app, ui| {
            if ui.button(i18n::tr(K::RefreshNow)).clicked() {
                ui.close();
            }
        },
    );

    // Live search filter from the details tab itself.
    if !app.details_state.filter.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.label(
                egui::RichText::new(format!("\"{}\"", app.details_state.filter))
                    .size(12.5)
                    .color(pal.accent),
            );
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("✕").size(12.0).color(pal.text_dim),
                    )
                    .frame(false),
                )
                .clicked()
            {
                app.details_state.filter.clear();
            }
        });
    }

    let mut table = app.make_table("details", columns(), 340.0);

    // Rebuild the row model only when the snapshot/search/sort/lang changes.
    let key = (
        snap.timestamp_ms,
        search_text(app),
        app.details_state.sort_col,
        app.details_state.ascending,
        app.lang() as u8,
    );
    let mut cache = app.details_state.cache.take();
    let stale = cache.as_ref().is_none_or(|c| c.key != key);
    if stale {
        cache = Some(Cache {
            key: key.clone(),
            rows: build_rows(&snap, &key.1, key.2, key.3),
        });
    }
    let rows = &cache.as_ref().expect("cache").rows;

    let avail = crate::widgets::tablekit::table_avail(ui);
    if let Some(col) = table.header(
        ui,
        &pal,
        avail,
        Some((app.details_state.sort_col, app.details_state.ascending)),
        None,
    ) {
        if app.details_state.sort_col == col {
            app.details_state.ascending = !app.details_state.ascending;
        } else {
            app.details_state.sort_col = col;
            app.details_state.ascending = col == 0 || !table.cols[col.min(table.cols.len() - 1)].numeric;
        }
    }

    egui::ScrollArea::vertical()
        .id_salt("details-table")
        .auto_shrink(false)
        .show(ui, |ui| {
            for row in rows {
                let selected = app.selected_pid == Some(row.pid);
                let (rect, resp) = table.row(ui, &pal, avail, selected);

                let tex = row
                    .icon_path
                    .as_ref()
                    .and_then(|p| app.shared.icons.get(ui.ctx(), app.actions.as_ref(), p, 6));
                table.icon_cell(ui, rect, tex.as_ref(), pal.accent);
                let name_rect = table.col_rect(0, avail, rect);
                ui.painter().text(
                    egui::Pos2::new(name_rect.left() + 56.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &row.name,
                    egui::FontId::proportional(12.5),
                    pal.text,
                );

                table.text_cell(ui, avail, rect, 1, &row.pid_s, &pal, false);
                table.text_cell(ui, avail, rect, 2, row.status, &pal, false);
                table.text_cell(ui, avail, rect, 3, &row.user, &pal, false);
                table.text_cell(ui, avail, rect, 4, &row.cpu_s, &pal, false);
                // Memory: right-aligned like TM.
                let mem_rect = table.col_rect(5, avail, rect);
                ui.painter().text(
                    egui::Pos2::new(mem_rect.right() - 10.0, rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    &row.mem_s,
                    egui::FontId::proportional(12.5),
                    pal.text,
                );
                table.text_cell(ui, avail, rect, 6, row.platform, &pal, false);
                table.text_cell(ui, avail, rect, 7, row.elevated, &pal, false);
                table.text_cell(ui, avail, rect, 8, row.uac, &pal, false);
                table.text_cell(ui, avail, rect, 9, "", &pal, false);

                if resp.clicked() {
                    app.selected_pid = Some(row.pid);
                }
                resp.context_menu(|ui| {
                    // Re-fetch the live entry for the context menu actions.
                    if let Some(p) = snap.process(row.pid) {
                        context_menu(app, ui, p);
                    }
                });
            }
            ui.add_space(12.0);
        });
    app.persist_table(&table);
    app.details_state.cache = cache;
}

/// Effective filter text (global search + the "go to details" jump).
fn search_text(app: &TaskManApp) -> String {
    if !app.details_state.filter.is_empty() {
        app.details_state.filter.clone()
    } else {
        app.search.clone()
    }
}

fn build_rows(
    snap: &tm_core::model::Snapshot,
    search: &str,
    sort_col: usize,
    ascending: bool,
) -> Vec<Row> {
    let q = search.trim().to_lowercase();
    let mut list: Vec<&ProcessEntry> = snap
        .processes
        .iter()
        .filter(|p| {
            q.is_empty()
                || p.name.to_lowercase().contains(&q)
                || p.shown_name().to_lowercase().contains(&q)
        })
        .collect();

    let sv = |p: &ProcessEntry, i: usize| -> f64 {
        match i {
            4 => p.cpu_pct as f64,
            5 => p.mem_bytes as f64,
            _ => 0.0,
        }
    };
    list.sort_by(|a, b| {
        let o = match sort_col {
            1 => a.pid.cmp(&b.pid),
            2 => status_rank(a.status).cmp(&status_rank(b.status)),
            3 => a.user.as_deref().cmp(&b.user.as_deref()),
            4 => sv(a, 4)
                .partial_cmp(&sv(b, 4))
                .unwrap_or(std::cmp::Ordering::Equal),
            5 => sv(a, 5)
                .partial_cmp(&sv(b, 5))
                .unwrap_or(std::cmp::Ordering::Equal),
            6 => a.wow64.cmp(&b.wow64),
            7 => a.elevated.cmp(&b.elevated),
            8 => a.priority.cmp(&b.priority),
            _ => a
                .shown_name()
                .to_lowercase()
                .cmp(&b.shown_name().to_lowercase()),
        };
        if ascending { o } else { o.reverse() }
    });

    list.into_iter()
        .map(|p| {
            let status = match p.status {
                ProcStatus::Running => i18n::tr(K::StRunning),
                ProcStatus::Suspended => i18n::tr(K::StSuspended),
                ProcStatus::NotResponding => i18n::tr(K::StNotResponding),
            };
            let platform = match p.wow64 {
                Some(true) => i18n::tr(K::Bit32),
                _ => i18n::tr(K::Bit64),
            };
            let elevated = match p.elevated {
                Some(true) => i18n::tr(K::Yes),
                _ if p.user.as_deref() == Some("SYSTEM") || matches!(p.pid, 4 | 0) => {
                    i18n::tr(K::Yes)
                }
                _ => i18n::tr(K::No),
            };
            let uac = if p.user.as_deref() == Some("SYSTEM") || matches!(p.pid, 4 | 0) {
                i18n::tr(K::NotAllowed)
            } else {
                i18n::tr(K::DisabledWord)
            };
            Row {
                pid: p.pid,
                name: p.shown_name().to_string(),
                icon_path: p
                    .exe_path
                    .as_ref()
                    .map(|x| x.to_string_lossy().into_owned()),
                pid_s: p.pid.to_string(),
                status,
                user: p.user.clone().unwrap_or_default(),
                cpu_s: format::format_cpu_detail(p.cpu_pct),
                mem_s: format::format_k(p.mem_bytes),
                platform,
                elevated,
                uac,
            }
        })
        .collect()
}

fn status_rank(s: ProcStatus) -> u8 {
    match s {
        ProcStatus::Running => 0,
        ProcStatus::Suspended => 1,
        ProcStatus::NotResponding => 2,
    }
}

/// Context menu mirroring the Win11 TM Details tab.
pub fn context_menu(app: &mut TaskManApp, ui: &mut egui::Ui, p: &ProcessEntry) {
    ui.set_min_width(230.0);
    ui.label(
        egui::RichText::new(p.shown_name())
            .strong()
            .size(12.5),
    );
    ui.separator();

    if ui.button(i18n::tr(K::CopyName)).clicked() {
        ui.ctx().copy_text(p.shown_name().to_string());
        app.shared.toast(i18n::tr(K::Copied));
        ui.close();
    }
    if ui.button(i18n::tr(K::EndTask)).clicked() {
        end_process(app, p.pid, false, p.shown_name());
        ui.close();
    }
    #[cfg(target_os = "windows")]
    if ui.button(i18n::tr(K::EndTree)).clicked() {
        end_process(app, p.pid, true, p.shown_name());
        ui.close();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = p.name.as_str();
    }

    ui.menu_button(i18n::tr(K::Priority), |ui| {
        for (cls, key) in [
            (PriorityClass::Realtime, K::PrioRealtime),
            (PriorityClass::High, K::PrioHigh),
            (PriorityClass::AboveNormal, K::PrioAboveNormal),
            (PriorityClass::Normal, K::PrioNormal),
            (PriorityClass::BelowNormal, K::PrioBelowNormal),
            (PriorityClass::Low, K::PrioLow),
        ] {
            if ui.button(i18n::tr(key)).clicked() {
                match app.actions.set_priority(p.pid, cls) {
                    Ok(()) => app.shared.toast(i18n::trf(K::PrioritySetMsg, &[i18n::tr(key)])),
                    Err(e) => app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()])),
                }
                ui.close();
            }
        }
    });

    if ui.button(i18n::tr(K::SetAffinity)).clicked() {
        let mask = app.actions.get_affinity_mask(p.pid).unwrap_or(u64::MAX);
        app.affinity_dialog = Some((p.pid, mask));
        ui.close();
    }

    let suspended = p.status == ProcStatus::Suspended;
    if ui
        .button(if suspended {
            i18n::tr(K::ResumeProc)
        } else {
            i18n::tr(K::SuspendProc)
        })
        .clicked()
    {
        match app.actions.suspend_process(p.pid, !suspended) {
            Ok(()) => {}
            Err(e) => app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()])),
        }
        ui.close();
    }

    ui.separator();

    #[cfg(target_os = "windows")]
    {
        let eco_on = app.efficiency_pids.contains(&p.pid);
        if ui
            .button(if eco_on {
                i18n::tr(K::EfficiencyModeOff)
            } else {
                i18n::tr(K::EfficiencyModeOn)
            })
            .clicked()
        {
            match app.actions.set_efficiency_mode(p.pid, !eco_on) {
                Ok(()) => {
                    if eco_on {
                        app.efficiency_pids.remove(&p.pid);
                    } else {
                        app.efficiency_pids.insert(p.pid);
                    }
                    app.shared.toast(i18n::tr(K::EfficiencyChanged));
                }
                Err(e) => app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()])),
            }
            ui.close();
        }
        // Jump to the service hosted by this process (svchost.exe etc.).
        if ui.button(i18n::tr(K::GoToServices)).clicked() {
            app.goto_services_for_pid(p.pid);
            ui.close();
        }
    }

    if let Some(path) = p.exe_path.as_ref().map(|x| x.to_string_lossy().into_owned()) {
        if ui.button(i18n::tr(K::OpenFileLocation)).clicked() {
            if let Err(e) = app.actions.open_file_location(&path) {
                app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()]));
            }
            ui.close();
        }
        if ui.button(i18n::tr(K::CreateDumpFile)).clicked() {
            create_dump(app, p);
            ui.close();
        }
        if ui.button(i18n::tr(K::Properties)).clicked() {
            if let Err(e) = app.actions.open_properties(&path) {
                // Fall back to our own read-only dialog.
                tracing::debug!(error = %e, "shell properties failed; using built-in dialog");
                app.proc_props = Some(p.pid);
            }
            ui.close();
        }
    }
}

fn end_process(app: &mut TaskManApp, pid: u32, tree: bool, name: &str) {
    match app.actions.kill_process(pid, tree) {
        Ok(()) => app.shared.toast(if tree {
            i18n::trf(K::TreeOfEndedToast, &[name])
        } else {
            i18n::trf(K::NameEndedToast, &[name])
        }),
        Err(e) => app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()])),
    }
}

/// Ask where to save, then write a minidump on a worker thread.
fn create_dump(app: &mut TaskManApp, p: &ProcessEntry) {
    let default_name = format!("{}.dmp", p.shown_name());
    let Some(path) = rfd::FileDialog::new()
        .set_file_name(&default_name)
        .save_file()
    else {
        return;
    };
    let actions = app.actions.clone();
    let toasts = app.shared.toasts.clone();
    let pid = p.pid;
    let spawned = std::thread::Builder::new()
        .name("tm-dump".into())
        .spawn(move || {
            let msg = match actions.create_dump_file(pid, &path) {
                Ok(()) => i18n::trf(K::DumpWrittenMsg, &[&path.to_string_lossy()]),
                Err(e) => i18n::trf(K::ErrMsg, &[&e.to_string()]),
            };
            crate::app::toast_from(&toasts, msg);
        });
    if spawned.is_err() {
        app.shared.toast(i18n::tr(K::DumpFailed));
    }
}

/// Built-in read-only process properties dialog (fallback when the shell's
/// own Properties dialog is unavailable).
pub fn process_properties_dialog(app: &mut TaskManApp, ctx: &egui::Context) {
    let mut open = true;
    egui::Window::new(i18n::tr(K::Properties))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let pid = app.proc_props.unwrap_or(0);
            let entry = app.latest_snapshot().and_then(|s| s.process(pid).cloned());
            let Some(p) = entry else {
                ui.label(i18n::tr(K::ProcessExited));
                ui.add_space(8.0);
                if ui.button(i18n::tr(K::Close)).clicked() {
                    app.proc_props = None;
                }
                return;
            };
            ui.set_min_width(430.0);
            let status = match p.status {
                ProcStatus::Running => i18n::tr(K::StRunning),
                ProcStatus::Suspended => i18n::tr(K::StSuspended),
                ProcStatus::NotResponding => i18n::tr(K::StNotResponding),
            };
            let path = p
                .exe_path
                .as_ref()
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_else(|| i18n::tr(K::NoFileForProcess).to_string());
            egui::Grid::new("proc-props")
                .num_columns(2)
                .spacing([14.0, 5.0])
                .show(ui, |ui| {
                    ui.weak(i18n::tr(K::ColName));
                    ui.label(p.shown_name());
                    ui.end_row();
                    ui.weak(i18n::tr(K::ColPid));
                    ui.label(p.pid.to_string());
                    ui.end_row();
                    ui.weak(i18n::tr(K::ColStatus));
                    ui.label(status);
                    ui.end_row();
                    ui.weak(i18n::tr(K::ColUsername));
                    ui.label(p.user.clone().unwrap_or_default());
                    ui.end_row();
                    ui.weak(i18n::tr(K::ColPlatform));
                    ui.label(match p.wow64 {
                        Some(true) => i18n::tr(K::Bit32),
                        _ => i18n::tr(K::Bit64),
                    });
                    ui.end_row();
                    ui.weak(i18n::tr(K::PropPath));
                    ui.add(
                        egui::TextEdit::singleline(&mut path.clone())
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();
                });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(i18n::tr(K::OpenFileLocation)).clicked()
                    && let Err(e) = app.actions.open_file_location(&path)
                {
                    app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()]));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(i18n::tr(K::Close)).clicked() {
                        app.proc_props = None;
                    }
                });
            });
        });
    if !open {
        app.proc_props = None;
    }
}

/// Affinity checkbox dialog (up to 64 logical processors).
pub fn affinity_dialog(
    app: &mut TaskManApp,
    ctx: &egui::Context,
    pid: u32,
    mask: u64,
    _pal: &theme::Palette,
) {
    let mut open = true;
    egui::Window::new(format!(
        "{} {pid}",
        i18n::tr(K::AffinityTitle)
    ))
    .open(&mut open)
    .collapsible(false)
    .resizable(false)
    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
    .show(ctx, |ui| {
        let sys_mask = app.actions.system_affinity_mask().unwrap_or(u64::MAX);
        let mut new_mask = mask;
        egui::Grid::new("affinity")
            .num_columns(8)
            .spacing([6.0, 6.0])
            .show(ui, |ui| {
                for cpu in 0..64usize {
                    let allowed = sys_mask & (1u64 << cpu) != 0;
                    let mut on = mask & (1u64 << cpu) != 0;
                    if ui
                        .add_enabled(allowed, egui::Checkbox::new(&mut on, cpu.to_string()))
                        .changed()
                    {
                        if on {
                            new_mask |= 1u64 << cpu;
                        } else {
                            new_mask &= !(1u64 << cpu);
                        }
                    }
                    if (cpu + 1) % 8 == 0 {
                        ui.end_row();
                    }
                }
            });
        if new_mask == 0 {
            ui.label(
                egui::RichText::new(i18n::tr(K::AffinityWarn)).color(theme::DARK.heat_high),
            );
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button(i18n::tr(K::Cancel)).clicked() {
                app.affinity_dialog = None;
            }
            if ui
                .add_enabled(new_mask != 0, egui::Button::new(i18n::tr(K::Apply)))
                .clicked()
            {
                match app.actions.set_affinity_mask(pid, new_mask) {
                    Ok(()) => app.shared.toast(i18n::tr(K::AffinitySet)),
                    Err(e) => app.shared.toast(i18n::trf(K::ErrMsg, &[&e.to_string()])),
                }
                app.affinity_dialog = None;
            }
        });
    });
    if !open {
        app.affinity_dialog = None;
    }
}
