//! Floating Live Process Manager window with sortable table (`PID`, `Name`,
//! `CPU %`, `RSS MB`) and remote `Kill` (SIGTERM) / `SIGKILL` controls.

use std::time::{Duration, Instant};

use egui::{Color32, RichText, ScrollArea};
use pane_proto::{ClientMsg, ProcessInfo};

use crate::theme;

const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortCol {
    Pid,
    Name,
    Cpu,
    Rss,
}

pub struct ProcessManager {
    pub open: bool,
    pub processes: Vec<ProcessInfo>,
    pub filter: String,
    sort_col: SortCol,
    sort_desc: bool,
    last_refresh: Option<Instant>,
    pub last_status: Option<String>,
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            open: false,
            processes: Vec::new(),
            filter: String::new(),
            sort_col: SortCol::Cpu,
            sort_desc: true,
            last_refresh: None,
            last_status: None,
        }
    }

    pub fn request_refresh(&mut self, req_id: u64, out: &mut Vec<ClientMsg>) {
        self.last_refresh = Some(Instant::now());
        out.push(ClientMsg::ListProcesses { req_id });
    }

    pub fn maybe_auto_refresh(&mut self, alloc_req_id: &mut impl FnMut() -> u64, out: &mut Vec<ClientMsg>) {
        if !self.open {
            return;
        }
        let due = self.last_refresh.is_none_or(|t| t.elapsed() >= AUTO_REFRESH_INTERVAL);
        if due {
            let req_id = alloc_req_id();
            self.request_refresh(req_id, out);
        }
    }

    pub fn on_list(&mut self, processes: Vec<ProcessInfo>) {
        self.processes = processes;
        self.apply_sort();
    }

    fn toggle_sort(&mut self, col: SortCol) {
        if self.sort_col == col {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort_col = col;
            self.sort_desc = matches!(col, SortCol::Cpu | SortCol::Rss);
        }
        self.apply_sort();
    }

    fn apply_sort(&mut self) {
        let desc = self.sort_desc;
        let col = self.sort_col;
        self.processes.sort_by(|a, b| {
            let ord = match col {
                SortCol::Pid => a.pid.cmp(&b.pid),
                SortCol::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortCol::Cpu => a.cpu_percent.total_cmp(&b.cpu_percent),
                SortCol::Rss => a.rss_mb.total_cmp(&b.rss_mb),
            };
            let ord = if desc { ord.reverse() } else { ord };
            ord.then_with(|| a.pid.cmp(&b.pid))
        });
    }

    pub fn show(&mut self, ui: &mut egui::Ui, alloc_req_id: &mut impl FnMut() -> u64, out: &mut Vec<ClientMsg>) {
        // Toolbar: filter, count, manual refresh.
        ui.horizontal(|ui| {
            ui.add_sized(
                [180.0, 20.0],
                egui::TextEdit::singleline(&mut self.filter).hint_text("Filter PID or name..."),
            );
            if ui.button("Refresh").clicked() {
                let req_id = alloc_req_id();
                self.request_refresh(req_id, out);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{} shown (2s refresh)", self.processes.len()))
                        .small()
                        .color(theme::TEXT_MUTED),
                );
            });
        });

        if let Some(status) = &self.last_status {
            ui.label(RichText::new(status).small().color(theme::ACCENT_AMBER));
        }

        ui.separator();

        // Sortable column header row.
        let header_label = |col: SortCol, title: &str| -> String {
            if self.sort_col == col {
                format!("{title} {}", if self.sort_desc { "v" } else { "^" })
            } else {
                title.to_owned()
            }
        };

        let mut clicked_sort: Option<SortCol> = None;
        let mut kill_req: Option<(u32, i32, String)> = None;
        let filter_lc = self.filter.to_lowercase();

        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("proc_table_grid")
                .num_columns(5)
                .striped(true)
                .spacing([12.0, 4.0])
                .min_col_width(48.0)
                .show(ui, |ui| {
                    if ui.button(header_label(SortCol::Pid, "PID")).clicked() {
                        clicked_sort = Some(SortCol::Pid);
                    }
                    if ui.button(header_label(SortCol::Name, "Name")).clicked() {
                        clicked_sort = Some(SortCol::Name);
                    }
                    if ui.button(header_label(SortCol::Cpu, "CPU %")).clicked() {
                        clicked_sort = Some(SortCol::Cpu);
                    }
                    if ui.button(header_label(SortCol::Rss, "RSS MB")).clicked() {
                        clicked_sort = Some(SortCol::Rss);
                    }
                    ui.label(RichText::new("Action").strong().size(11.5).color(theme::TEXT_MUTED));
                    ui.end_row();

                    for p in &self.processes {
                        if !filter_lc.is_empty()
                            && !p.name.to_lowercase().contains(&filter_lc)
                            && !p.pid.to_string().contains(&filter_lc)
                        {
                            continue;
                        }
                        ui.label(RichText::new(format!("{:>6}", p.pid)).monospace().size(11.5).color(theme::TEXT_MUTED));
                        ui.label(RichText::new(&p.name).monospace().size(12.0).color(theme::TEXT_PRIMARY));

                        let cpu_col = if p.cpu_percent > 50.0 {
                            theme::ACCENT_CORAL
                        } else if p.cpu_percent > 15.0 {
                            theme::ACCENT_AMBER
                        } else {
                            theme::ACCENT_CYAN
                        };
                        ui.label(RichText::new(format!("{:5.1}%", p.cpu_percent)).monospace().size(11.5).color(cpu_col));

                        let rss_col = if p.rss_mb > 512.0 { theme::ACCENT_VIOLET } else { Color32::from_rgb(196, 206, 220) };
                        ui.label(RichText::new(format!("{:7.1}", p.rss_mb)).monospace().size(11.5).color(rss_col));

                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            if ui
                                .small_button(RichText::new("Kill").color(theme::ACCENT_CORAL))
                                .on_hover_text("Send SIGTERM (15)")
                                .clicked()
                            {
                                kill_req = Some((p.pid, 15, p.name.clone()));
                            }
                            if ui
                                .small_button(RichText::new("-9").color(theme::ACCENT_AMBER))
                                .on_hover_text("Force kill: SIGKILL (9)")
                                .clicked()
                            {
                                kill_req = Some((p.pid, 9, p.name.clone()));
                            }
                        });
                        ui.end_row();
                    }
                });
        });

        if let Some(col) = clicked_sort {
            self.toggle_sort(col);
        }
        if let Some((pid, signal, name)) = kill_req {
            let req_id = alloc_req_id();
            self.last_status = Some(format!("Sent signal {signal} to {name} (PID {pid})"));
            out.push(ClientMsg::KillProcess { req_id, pid, signal });
        }
    }
}
