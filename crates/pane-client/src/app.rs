//! Root egui workspace application: top HUD bar, dockable/floating File
//! Explorer and System Monitor, and multiple floating remote Terminal windows.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use egui::{CentralPanel, Color32, Frame, Margin, Panel, Pos2, RichText, Stroke, Vec2, Window};
use pane_proto::{ClientMsg, HostMsg, PROTOCOL_VERSION};

use crate::files_view::FileExplorer;
use crate::metrics::{SessionMetrics, fmt_bytes};
use crate::monitor_view::{HostInfo, SystemMonitor, TelemetryState};
use crate::terminal_view::{TerminalPane, show_terminal};
use crate::theme;
use crate::transport::{LaunchMode, Transport, TransportEvent};

const PING_INTERVAL: Duration = Duration::from_secs(1);

pub struct PaneApp {
    mode: LaunchMode,
    transport: Option<Transport>,
    host: Option<HostInfo>,
    telemetry: TelemetryState,
    metrics: SessionMetrics,

    terminals: Vec<TerminalPane>,
    next_term_id: u32,
    focused_term: Option<u32>,

    files: FileExplorer,
    monitor: SystemMonitor,

    status_banner: Option<(String, Color32, Instant)>,
    disconnect_reason: Option<String>,
    stderr_tail: VecDeque<String>,
}

impl PaneApp {
    pub fn new(ctx: egui::Context, mode: LaunchMode) -> Self {
        let mut app = Self {
            mode: mode.clone(),
            transport: None,
            host: None,
            telemetry: TelemetryState::default(),
            metrics: SessionMetrics::new(),
            terminals: Vec::new(),
            next_term_id: 1,
            focused_term: None,
            files: FileExplorer::new(),
            monitor: SystemMonitor::new(),
            status_banner: None,
            disconnect_reason: None,
            stderr_tail: VecDeque::with_capacity(12),
        };

        match Transport::connect(&mode, ctx) {
            Ok(t) => {
                t.send(ClientMsg::Hello { protocol_version: PROTOCOL_VERSION });
                t.send(ClientMsg::Ping { timestamp_micros: app.metrics.elapsed_micros() });
                app.metrics.last_ping_sent = Some(Instant::now());
                app.transport = Some(t);
            }
            Err(e) => {
                app.disconnect_reason = Some(format!("{e:#}"));
            }
        }
        app
    }

    fn send(&self, msg: ClientMsg) {
        if let Some(t) = &self.transport {
            t.send(msg);
        }
    }

    fn spawn_terminal(&mut self) {
        let id = self.next_term_id;
        self.next_term_id += 1;
        let (cols, rows) = (96, 28);
        self.terminals.push(TerminalPane::new(id, cols, rows));
        self.focused_term = Some(id);
        self.send(ClientMsg::OpenTerminal { term_id: id, cols, rows });
    }

    fn notify(&mut self, msg: String, color: Color32) {
        self.status_banner = Some((msg, color, Instant::now()));
    }

    fn poll_transport(&mut self) {
        let mut outgoing = Vec::new();
        let events: Vec<TransportEvent> = self
            .transport
            .as_ref()
            .map(|t| t.rx.try_iter().collect())
            .unwrap_or_default();

        for ev in events {
            match ev {
                TransportEvent::Sent { wire_bytes } => self.metrics.record_tx(wire_bytes),
                TransportEvent::Stderr(line) => {
                    if self.stderr_tail.len() == 12 {
                        self.stderr_tail.pop_front();
                    }
                    self.stderr_tail.push_back(line);
                }
                TransportEvent::Disconnected(reason) => {
                    self.disconnect_reason = Some(reason);
                    self.transport = None;
                }
                TransportEvent::Msg { msg, wire_bytes } => {
                    self.metrics.record_rx(wire_bytes);
                    self.handle_host_msg(msg, &mut outgoing);
                }
            }
        }

        for m in outgoing {
            self.send(m);
        }

        // Send periodic 1 Hz RTT ping while connected.
        if self.transport.is_some()
            && self.metrics.last_ping_sent.is_none_or(|t| t.elapsed() >= PING_INTERVAL)
        {
            self.metrics.last_ping_sent = Some(Instant::now());
            self.send(ClientMsg::Ping { timestamp_micros: self.metrics.elapsed_micros() });
        }
        self.metrics.tick();
    }

    fn handle_host_msg(&mut self, msg: HostMsg, out: &mut Vec<ClientMsg>) {
        match msg {
            HostMsg::Welcome { hostname, os_info, cpu_cores, total_mem_mb, home_dir, .. } => {
                self.host = Some(HostInfo { hostname, os_info, cpu_cores, total_mem_mb });
                self.files.request_dir(home_dir, out);
                if self.terminals.is_empty() {
                    let id = self.next_term_id;
                    self.next_term_id += 1;
                    self.terminals.push(TerminalPane::new(id, 96, 28));
                    self.focused_term = Some(id);
                    out.push(ClientMsg::OpenTerminal { term_id: id, cols: 96, rows: 28 });
                }
            }
            HostMsg::Pong { timestamp_micros } => self.metrics.record_pong(timestamp_micros),
            HostMsg::TermDiff {
                term_id,
                screen_cols,
                screen_rows,
                cursor_row,
                cursor_col,
                cursor_visible,
                app_cursor,
                bracketed_paste,
                scrollback,
                rows,
            } => {
                if let Some(t) = self.terminals.iter_mut().find(|t| t.id == term_id) {
                    t.screen.apply(screen_cols, screen_rows, rows);
                    t.screen.cursor_row = cursor_row;
                    t.screen.cursor_col = cursor_col;
                    t.screen.cursor_visible = cursor_visible;
                    t.screen.app_cursor = app_cursor;
                    t.screen.bracketed_paste = bracketed_paste;
                    t.screen.scrollback = scrollback;
                }
            }
            HostMsg::TermTitle { term_id, title } => {
                if let Some(t) = self.terminals.iter_mut().find(|t| t.id == term_id) {
                    t.title = if title.is_empty() { format!("Terminal #{term_id}") } else { title };
                }
            }
            HostMsg::TermClosed { term_id, exit_code } => {
                if let Some(t) = self.terminals.iter_mut().find(|t| t.id == term_id) {
                    t.closed = true;
                    t.exit_code = exit_code;
                }
            }
            HostMsg::DirListing { path, entries, .. } => self.files.on_listing(path, entries),
            HostMsg::FileChunk { req_id, total_size, data, eof, .. } => {
                if let Some(notice) = self.files.on_chunk(req_id, total_size, &data, eof) {
                    self.notify(notice, theme::ACCENT_GREEN);
                }
            }
            HostMsg::TelemetryUpdate { cpu_percent, mem_used_mb, load_avg, per_core, uptime_secs } => {
                let total_mem = self.host.as_ref().map(|h| h.total_mem_mb).unwrap_or(1).max(1);
                let mem_pct = (mem_used_mb as f32 / total_mem as f32) * 100.0;
                self.metrics.record_telemetry(cpu_percent, mem_pct);
                self.telemetry = TelemetryState {
                    cpu_percent,
                    mem_used_mb,
                    load_avg,
                    per_core,
                    uptime_secs,
                    received: true,
                };
            }
            HostMsg::Error { req_id, message } => {
                self.files.on_error(req_id, message.clone());
                self.notify(message, theme::ACCENT_CORAL);
            }
        }
    }

    fn show_hud(&mut self, ui: &mut egui::Ui) {
        let hud_frame = Frame::new()
            .fill(theme::BG_HUD)
            .stroke(Stroke::new(1.0, theme::BORDER))
            .inner_margin(Margin::symmetric(12, 7));

        Panel::top("pane_hud").frame(hud_frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                // Brand & transport mode badge.
                ui.label(RichText::new("PANE").monospace().strong().size(14.0).color(theme::ACCENT_CYAN));
                let (badge, badge_col) = if self.mode.is_demo() {
                    ("DEMO", theme::ACCENT_AMBER)
                } else {
                    ("SSH", theme::ACCENT_GREEN)
                };
                ui.label(RichText::new(format!("[{badge}]")).monospace().size(11.0).color(badge_col));

                ui.separator();

                // Connected host info.
                let connected = self.transport.is_some() && self.host.is_some();
                let dot_color = if connected { theme::ACCENT_GREEN } else { theme::ACCENT_CORAL };
                ui.label(RichText::new("●").color(dot_color));
                if let Some(h) = &self.host {
                    ui.label(RichText::new(&h.hostname).strong().color(theme::TEXT_PRIMARY))
                        .on_hover_text(&h.os_info);
                } else {
                    ui.label(RichText::new(self.mode.label()).color(theme::TEXT_MUTED));
                }

                ui.separator();

                // Live RTT ping.
                let (rtt_text, rtt_col) = match self.metrics.rtt_ms {
                    Some(ms) if ms < 30.0 => (format!("{ms:.1} ms"), theme::ACCENT_GREEN),
                    Some(ms) if ms < 100.0 => (format!("{ms:.1} ms"), theme::ACCENT_AMBER),
                    Some(ms) => (format!("{ms:.0} ms"), theme::ACCENT_CORAL),
                    None => ("— ms".into(), theme::TEXT_MUTED),
                };
                hud_pill(ui, "RTT", &rtt_text, rtt_col);

                // Real-time compressed bandwidth rate.
                hud_pill(ui, "RATE", &format!("{:.1} KB/s", self.metrics.bandwidth_kb_per_sec()), theme::ACCENT_CYAN);

                // Total bytes transferred.
                hud_pill(ui, "TOTAL", &fmt_bytes(self.metrics.total_bytes() as f64), theme::TEXT_PRIMARY);

                // Live Bandwidth Saved vs. 1080p60 VNC (12 MB/s baseline).
                let saved = format!(
                    "{} ({:.2}%)",
                    fmt_bytes(self.metrics.bytes_saved_vs_vnc()),
                    self.metrics.saved_percent_vs_vnc()
                );
                hud_pill(ui, "SAVED vs 1080p60 VNC", &saved, theme::ACCENT_GREEN);

                // Right-aligned workspace launchers.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.selectable_label(self.monitor.open, "Monitor").clicked() {
                        self.monitor.open = !self.monitor.open;
                    }
                    if ui.selectable_label(self.files.open, "Files").clicked() {
                        self.files.open = !self.files.open;
                    }
                    if ui.button(RichText::new("+ Terminal").color(theme::ACCENT_CYAN).strong()).clicked() {
                        self.spawn_terminal();
                    }
                });
            });
        });
    }
}

impl eframe::App for PaneApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_transport();
        let ctx = ui.ctx().clone();
        // Keep HUD counters (VNC savings & RTT) ticking smoothly every 250 ms even when idle.
        ctx.request_repaint_after(Duration::from_millis(250));

        self.show_hud(ui);

        let mut outgoing = Vec::new();

        // Docked File Explorer (left).
        if self.files.open && self.files.docked {
            Panel::left("files_dock")
                .resizable(true)
                .default_size(270.0)
                .min_size(200.0)
                .frame(panel_frame())
                .show(ui, |ui| {
                    ui.label(RichText::new("REMOTE FILES").small().strong().color(theme::TEXT_MUTED));
                    self.files.show(ui, &mut outgoing);
                });
        }

        // Docked System Monitor (right).
        if self.monitor.open && self.monitor.docked {
            Panel::right("monitor_dock")
                .resizable(true)
                .default_size(285.0)
                .min_size(230.0)
                .frame(panel_frame())
                .show(ui, |ui| {
                    ui.label(RichText::new("SYSTEM MONITOR").small().strong().color(theme::TEXT_MUTED));
                    self.monitor.show(ui, self.host.as_ref(), &self.telemetry, &self.metrics);
                });
        }

        // Central workspace canvas with subtle grid dots.
        CentralPanel::no_frame()
            .frame(Frame::new().fill(theme::BG_CANVAS))
            .show(ui, |ui| {
                paint_canvas_grid(ui);

                if let Some(reason) = &self.disconnect_reason {
                    ui.centered_and_justified(|ui| {
                        Frame::new()
                            .fill(theme::BG_PANEL)
                            .stroke(Stroke::new(1.0, theme::ACCENT_CORAL))
                            .corner_radius(6.0)
                            .inner_margin(16.0)
                            .show(ui, |ui| {
                                ui.set_max_width(520.0);
                                ui.label(RichText::new("Disconnected").strong().size(16.0).color(theme::ACCENT_CORAL));
                                ui.add_space(4.0);
                                ui.label(RichText::new(reason).monospace());
                                if !self.stderr_tail.is_empty() {
                                    ui.add_space(8.0);
                                    ui.label(RichText::new("Remote stderr:").small().color(theme::TEXT_MUTED));
                                    for line in &self.stderr_tail {
                                        ui.label(RichText::new(line).monospace().small().color(theme::TEXT_MUTED));
                                    }
                                }
                            });
                    });
                }

                // Transient bottom-left status toast (auto-fades after 6s).
                if let Some((text, color, when)) = &self.status_banner {
                    if when.elapsed() < Duration::from_secs(6) {
                        let rect = ui.max_rect();
                        egui::Area::new(egui::Id::unique("pane_toast"))
                            .fixed_pos(Pos2::new(rect.left() + 14.0, rect.bottom() - 36.0))
                            .show(&ctx, |ui| {
                                Frame::new()
                                    .fill(theme::BG_PANEL)
                                    .stroke(Stroke::new(1.0, *color))
                                    .corner_radius(4.0)
                                    .inner_margin(Margin::symmetric(10, 5))
                                    .show(ui, |ui| {
                                        ui.label(RichText::new(text).monospace().small().color(*color));
                                    });
                            });
                    } else {
                        self.status_banner = None;
                    }
                }
            });

        // Floating File Explorer window when undocked.
        if self.files.open && !self.files.docked {
            let mut open = self.files.open;
            Window::new("Remote Files")
                .id(egui::Id::unique("win_files"))
                .open(&mut open)
                .default_pos([40.0, 64.0])
                .default_size([320.0, 520.0])
                .resizable(true)
                .show(&ctx, |ui| self.files.show(ui, &mut outgoing));
            self.files.open = open;
        }

        // Floating System Monitor window when undocked.
        if self.monitor.open && !self.monitor.docked {
            let mut open = self.monitor.open;
            Window::new("System Monitor")
                .id(egui::Id::unique("win_monitor"))
                .open(&mut open)
                .default_pos([980.0, 64.0])
                .default_size([310.0, 520.0])
                .resizable(true)
                .show(&ctx, |ui| {
                    self.monitor.show(ui, self.host.as_ref(), &self.telemetry, &self.metrics);
                });
            self.monitor.open = open;
        }

        // Floating remote Terminal windows.
        let mut closed_ids = Vec::new();
        for (idx, term) in self.terminals.iter_mut().enumerate() {
            let term_id = term.id;
            let mut open = term.open;
            let is_focused = self.focused_term == Some(term_id);
            let offset = (idx as f32 % 8.0) * 28.0;
            let title = format!("⌘ {}", term.title);
            let frame = Frame::window(&ctx.global_style())
                .fill(theme::BG_PANEL)
                .stroke(Stroke::new(1.0, if is_focused { theme::BORDER_FOCUS } else { theme::BORDER }))
                .inner_margin(Margin::same(4));

            let resp = Window::new(title)
                .id(egui::Id::unique(("term_win", term_id)))
                .open(&mut open)
                .frame(frame)
                .default_pos([295.0 + offset, 56.0 + offset])
                .default_size(Vec2::new(790.0, 500.0))
                .min_size(Vec2::new(260.0, 140.0))
                .resizable(true)
                .show(&ctx, |ui| show_terminal(ui, term, is_focused, &mut outgoing));

            if resp.as_ref().and_then(|r| r.inner).unwrap_or(false)
                || resp.as_ref().is_some_and(|r| r.response.clicked())
            {
                self.focused_term = Some(term_id);
            }
            term.open = open;
            if !open {
                closed_ids.push(term_id);
            }
        }

        for id in closed_ids {
            self.terminals.retain(|t| t.id != id);
            outgoing.push(ClientMsg::CloseTerminal { term_id: id });
            if self.focused_term == Some(id) {
                self.focused_term = self.terminals.last().map(|t| t.id);
            }
        }

        for msg in outgoing {
            self.send(msg);
        }
    }
}

fn panel_frame() -> Frame {
    Frame::new()
        .fill(theme::BG_PANEL)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .inner_margin(Margin::same(10))
}

fn hud_pill(ui: &mut egui::Ui, label: &str, value: &str, value_color: Color32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(RichText::new(label).size(10.5).color(theme::TEXT_MUTED));
        ui.label(RichText::new(value).monospace().strong().size(11.5).color(value_color));
    });
}

fn paint_canvas_grid(ui: &egui::Ui) {
    let rect = ui.max_rect();
    let p = ui.painter_at(rect);
    let step = 28.0;
    let dot_color = Color32::from_rgb(24, 31, 44);
    let mut y = rect.top() + 14.0;
    while y < rect.bottom() {
        let mut x = rect.left() + 14.0;
        while x < rect.right() {
            p.circle_filled(Pos2::new(x, y), 1.0, dot_color);
            x += step;
        }
        y += step;
    }
}
