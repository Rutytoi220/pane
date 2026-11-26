//! Root egui desktop workspace:
//! - Top HUD bar (host info, RTT ping, live KB/s, total bytes, VNC savings)
//! - Dockable/floating File Explorer (left) & System Monitor (right)
//! - Constrained CentralPanel workspace for floating Terminal, File Viewer, and
//!   Process Manager windows (never overlapping top HUD or bottom taskbar)
//! - Bottom Desktop Dock / Taskbar with quick-launchers, Tile Windows, and
//!   one-click tabs for every open window.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use egui::{CentralPanel, Color32, Frame, LayerId, Margin, Panel, Pos2, Rect, RichText, Sense, Stroke, Vec2, Window};
use pane_proto::{ClientMsg, HostMsg, PROTOCOL_VERSION};

use crate::files_view::FileExplorer;
use crate::metrics::{SessionMetrics, fmt_bytes};
use crate::monitor_view::{HostInfo, SystemMonitor, TelemetryState};
use crate::processes_view::ProcessManager;
use crate::terminal_view::{TerminalPane, show_terminal};
use crate::theme;
use crate::transport::{LaunchMode, Transport, TransportEvent};
use crate::viewer_view::FileViewerPane;

const PING_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WinKey {
    Terminal(u32),
    Viewer(u64),
    Processes,
    Files,
    Monitor,
}

pub struct PaneApp {
    mode: LaunchMode,
    transport: Option<Transport>,
    host: Option<HostInfo>,
    telemetry: TelemetryState,
    metrics: SessionMetrics,

    terminals: Vec<TerminalPane>,
    next_term_id: u32,
    focused_term: Option<u32>,

    viewers: Vec<FileViewerPane>,
    next_viewer_id: u64,
    next_aux_req_id: u64,

    files: FileExplorer,
    monitor: SystemMonitor,
    processes: ProcessManager,

    /// Workspace rect captured from `CentralPanel` each frame.
    workspace_rect: Rect,
    /// Active floating window key (highlighted in taskbar).
    active_window: Option<WinKey>,
    /// Last rendered `LayerId` per floating window so taskbar clicks can raise it.
    window_layers: HashMap<WinKey, LayerId>,
    /// One-shot target rects applied on the next frame when `Tile Windows` is clicked.
    pending_tiles: HashMap<WinKey, Rect>,

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
            viewers: Vec::new(),
            next_viewer_id: 1,
            next_aux_req_id: 100_000,
            files: FileExplorer::new(),
            monitor: SystemMonitor::new(),
            processes: ProcessManager::new(),
            workspace_rect: Rect::from_min_size(Pos2::new(270.0, 36.0), Vec2::new(880.0, 780.0)),
            active_window: None,
            window_layers: HashMap::new(),
            pending_tiles: HashMap::new(),
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

    fn alloc_aux_req_id(&mut self) -> u64 {
        let id = self.next_aux_req_id;
        self.next_aux_req_id += 1;
        id
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
        self.active_window = Some(WinKey::Terminal(id));
        self.send(ClientMsg::OpenTerminal { term_id: id, cols, rows });
    }

    fn open_file_viewer(&mut self, ctx: &egui::Context, path: String, out: &mut Vec<ClientMsg>) {
        // Re-focus if already open.
        if let Some(existing) = self.viewers.iter_mut().find(|v| v.path == path) {
            existing.open = true;
            let key = WinKey::Viewer(existing.id);
            self.raise_window(ctx, key);
            return;
        }
        let id = self.next_viewer_id;
        self.next_viewer_id += 1;
        let req_id = self.alloc_aux_req_id();
        self.viewers.push(FileViewerPane::new(id, req_id, path.clone()));
        self.active_window = Some(WinKey::Viewer(id));
        self.focused_term = None;
        out.push(ClientMsg::ReadTextFile { req_id, path });
    }

    fn raise_window(&mut self, ctx: &egui::Context, key: WinKey) {
        self.active_window = Some(key);
        if let WinKey::Terminal(id) = key {
            self.focused_term = Some(id);
        } else {
            self.focused_term = None;
        }
        if let Some(&layer_id) = self.window_layers.get(&key) {
            ctx.move_to_top(layer_id);
        }
    }

    fn tile_floating_windows(&mut self) {
        let mut keys = Vec::new();
        for t in &self.terminals {
            if t.open {
                keys.push(WinKey::Terminal(t.id));
            }
        }
        for v in &self.viewers {
            if v.open {
                keys.push(WinKey::Viewer(v.id));
            }
        }
        if self.processes.open {
            keys.push(WinKey::Processes);
        }
        if self.files.open && !self.files.docked {
            keys.push(WinKey::Files);
        }
        if self.monitor.open && !self.monitor.docked {
            keys.push(WinKey::Monitor);
        }

        let n = keys.len();
        if n == 0 {
            return;
        }
        let pad = 10.0;
        let area = self.workspace_rect.shrink(pad);
        let cols = (n as f32).sqrt().ceil() as usize;
        let rows = n.div_ceil(cols);
        let tile_w = ((area.width() - pad * (cols.saturating_sub(1)) as f32) / cols as f32).max(220.0);
        let tile_h = ((area.height() - pad * (rows.saturating_sub(1)) as f32) / rows as f32).max(160.0);

        self.pending_tiles.clear();
        for (i, key) in keys.into_iter().enumerate() {
            let c = i % cols;
            let r = i / cols;
            let min = Pos2::new(area.left() + c as f32 * (tile_w + pad), area.top() + r as f32 * (tile_h + pad));
            // Give the last window remaining horizontal width on an uneven last row.
            let is_last_partial = r == rows - 1 && i == n - 1 && (n % cols) != 0;
            let w = if is_last_partial { (area.right() - min.x).max(tile_w) } else { tile_w };
            // Subtract ~28px for window titlebar/frame chrome so outer frame fits cleanly.
            let inner_h = (tile_h - 32.0).max(120.0);
            let inner_w = (w - 12.0).max(200.0);
            self.pending_tiles.insert(key, Rect::from_min_size(min, Vec2::new(inner_w, inner_h)));
        }
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

        if self.transport.is_some() {
            let mut next_id = self.next_aux_req_id;
            self.processes.maybe_auto_refresh(
                &mut || {
                    let id = next_id;
                    next_id += 1;
                    id
                },
                &mut outgoing,
            );
            self.next_aux_req_id = next_id;
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
                    self.active_window = Some(WinKey::Terminal(id));
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
            HostMsg::TextFileContent { req_id, path, content, truncated } => {
                if let Some(v) = self.viewers.iter_mut().find(|v| v.req_id == req_id || v.path == path) {
                    v.set_content(path, content, truncated);
                }
            }
            HostMsg::ProcessList { processes, .. } => self.processes.on_list(processes),
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
                if let Some(id) = req_id
                    && let Some(v) = self.viewers.iter_mut().find(|v| v.req_id == id)
                {
                    v.set_error(message.clone());
                } else {
                    self.files.on_error(req_id, message.clone());
                }
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

                // Connected host info with a painted status dot (no Unicode glyph).
                let connected = self.transport.is_some() && self.host.is_some();
                let dot_color = if connected { theme::ACCENT_GREEN } else { theme::ACCENT_CORAL };
                let (dot_rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                ui.painter().circle_filled(dot_rect.center(), 4.0, dot_color);

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
                    None => ("-- ms".into(), theme::TEXT_MUTED),
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
            });
        });
    }

    fn show_taskbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, out: &mut Vec<ClientMsg>) {
        let bar_frame = Frame::new()
            .fill(theme::BG_HUD)
            .stroke(Stroke::new(1.0, theme::BORDER))
            .inner_margin(Margin::symmetric(10, 5));

        Panel::bottom("pane_taskbar").frame(bar_frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                // Quick-launch buttons.
                if ui.button(RichText::new("+ Terminal").color(theme::ACCENT_CYAN).strong()).clicked() {
                    self.spawn_terminal();
                }
                if ui.selectable_label(self.files.open, "Files").clicked() {
                    self.files.open = !self.files.open;
                    if self.files.open && !self.files.docked {
                        self.raise_window(ctx, WinKey::Files);
                    }
                }
                if ui.selectable_label(self.processes.open, "Processes").clicked() {
                    self.processes.open = !self.processes.open;
                    if self.processes.open {
                        let req_id = self.alloc_aux_req_id();
                        self.processes.request_refresh(req_id, out);
                        self.raise_window(ctx, WinKey::Processes);
                    }
                }
                if ui.selectable_label(self.monitor.open, "Monitor").clicked() {
                    self.monitor.open = !self.monitor.open;
                    if self.monitor.open && !self.monitor.docked {
                        self.raise_window(ctx, WinKey::Monitor);
                    }
                }
                if ui.button("Tile Windows").on_hover_text("Arrange all floating windows in a clean grid").clicked() {
                    self.tile_floating_windows();
                }

                ui.separator();

                // Window switcher tabs for all open floating windows.
                let mut raise_target: Option<WinKey> = None;

                egui::ScrollArea::horizontal().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for t in &self.terminals {
                            let key = WinKey::Terminal(t.id);
                            let selected = self.active_window == Some(key);
                            let label = format!("[TERM] {}", truncate_label(&t.title, 20));
                            if ui.selectable_label(selected, RichText::new(label).monospace().size(11.5)).clicked() {
                                raise_target = Some(key);
                            }
                        }
                        for v in &self.viewers {
                            let key = WinKey::Viewer(v.id);
                            let selected = self.active_window == Some(key);
                            let label = format!("[VIEW] {}", truncate_label(&v.display_name, 20));
                            if ui.selectable_label(selected, RichText::new(label).monospace().size(11.5)).clicked() {
                                raise_target = Some(key);
                            }
                        }
                        if self.processes.open {
                            let selected = self.active_window == Some(WinKey::Processes);
                            if ui
                                .selectable_label(selected, RichText::new("[PROC] Processes").monospace().size(11.5))
                                .clicked()
                            {
                                raise_target = Some(WinKey::Processes);
                            }
                        }
                        if self.files.open && !self.files.docked {
                            let selected = self.active_window == Some(WinKey::Files);
                            if ui
                                .selectable_label(selected, RichText::new("[FILES] Files").monospace().size(11.5))
                                .clicked()
                            {
                                raise_target = Some(WinKey::Files);
                            }
                        }
                        if self.monitor.open && !self.monitor.docked {
                            let selected = self.active_window == Some(WinKey::Monitor);
                            if ui
                                .selectable_label(selected, RichText::new("[MON] Monitor").monospace().size(11.5))
                                .clicked()
                            {
                                raise_target = Some(WinKey::Monitor);
                            }
                        }
                    });
                });

                if let Some(key) = raise_target {
                    self.raise_window(ctx, key);
                }
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

        let mut outgoing = Vec::new();
        let mut preview_requests: Vec<String> = Vec::new();

        // 1. Top HUD bar & Bottom Taskbar reserved first so CentralPanel's rect excludes both.
        self.show_hud(ui);
        self.show_taskbar(ui, &ctx, &mut outgoing);

        // 2. Docked File Explorer (left).
        if self.files.open && self.files.docked {
            Panel::left("files_dock")
                .resizable(true)
                .default_size(270.0)
                .min_size(210.0)
                .frame(panel_frame())
                .show(ui, |ui| {
                    ui.label(RichText::new("REMOTE FILES").small().strong().color(theme::TEXT_MUTED));
                    if let Some(path) = self.files.show(ui, &mut outgoing) {
                        preview_requests.push(path);
                    }
                });
        }

        // 3. Docked System Monitor (right).
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

        // 4. Central desktop canvas: capture exact workspace bounds & constrain all floating windows to it.
        let mut workspace_rect = self.workspace_rect;
        CentralPanel::no_frame()
            .frame(Frame::new().fill(theme::BG_CANVAS))
            .show(ui, |ui| {
                workspace_rect = ui.max_rect();
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
                        egui::Area::new(egui::Id::unique("pane_toast"))
                            .fixed_pos(Pos2::new(workspace_rect.left() + 14.0, workspace_rect.bottom() - 36.0))
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
        self.workspace_rect = workspace_rect;

        for path in preview_requests {
            self.open_file_viewer(&ctx, path, &mut outgoing);
        }

        // 5. Floating File Explorer window when undocked (constrained to workspace_rect).
        if self.files.open && !self.files.docked {
            let mut open = self.files.open;
            let default_pos = Pos2::new(workspace_rect.left() + 14.0, workspace_rect.top() + 14.0);
            let default_size = Vec2::new(320.0, (workspace_rect.height() - 60.0).clamp(260.0, 560.0));
            let mut win = Window::new("[FILES] Remote Files")
                .id(egui::Id::unique("win_files"))
                .open(&mut open)
                .constrain_to(workspace_rect)
                .default_pos(default_pos)
                .default_size(default_size)
                .min_size(Vec2::new(220.0, 180.0))
                .resizable(true);
            win = apply_tile_rect(win, self.pending_tiles.remove(&WinKey::Files));
            let mut preview_from_float = None;
            let resp = win.show(&ctx, |ui| {
                preview_from_float = self.files.show(ui, &mut outgoing);
            });
            if let Some(r) = resp {
                self.window_layers.insert(WinKey::Files, r.response.layer_id);
                if r.response.clicked() {
                    self.active_window = Some(WinKey::Files);
                    self.focused_term = None;
                }
            }
            self.files.open = open;
            if let Some(p) = preview_from_float {
                self.open_file_viewer(&ctx, p, &mut outgoing);
            }
        }

        // 6. Floating System Monitor window when undocked (constrained to workspace_rect).
        if self.monitor.open && !self.monitor.docked {
            let mut open = self.monitor.open;
            let w = 310.0;
            let default_pos = Pos2::new((workspace_rect.right() - w - 16.0).max(workspace_rect.left() + 12.0), workspace_rect.top() + 14.0);
            let default_size = Vec2::new(w, (workspace_rect.height() - 60.0).clamp(280.0, 560.0));
            let mut win = Window::new("[MON] System Monitor")
                .id(egui::Id::unique("win_monitor"))
                .open(&mut open)
                .constrain_to(workspace_rect)
                .default_pos(default_pos)
                .default_size(default_size)
                .min_size(Vec2::new(240.0, 220.0))
                .resizable(true);
            win = apply_tile_rect(win, self.pending_tiles.remove(&WinKey::Monitor));
            let resp = win.show(&ctx, |ui| {
                self.monitor.show(ui, self.host.as_ref(), &self.telemetry, &self.metrics);
            });
            if let Some(r) = resp {
                self.window_layers.insert(WinKey::Monitor, r.response.layer_id);
                if r.response.clicked() {
                    self.active_window = Some(WinKey::Monitor);
                    self.focused_term = None;
                }
            }
            self.monitor.open = open;
        }

        // 7. Floating Live Process Manager window (constrained to workspace_rect).
        if self.processes.open {
            let mut open = self.processes.open;
            let w = (workspace_rect.width() * 0.68).clamp(440.0, 680.0);
            let h = (workspace_rect.height() * 0.68).clamp(320.0, 520.0);
            let default_pos = Pos2::new(
                workspace_rect.center().x - w * 0.5,
                workspace_rect.center().y - h * 0.5,
            );
            let is_active = self.active_window == Some(WinKey::Processes);
            let frame = window_frame(&ctx, is_active);
            let mut win = Window::new("[PROC] Processes")
                .id(egui::Id::unique("win_processes"))
                .open(&mut open)
                .frame(frame)
                .constrain_to(workspace_rect)
                .default_pos(default_pos)
                .default_size(Vec2::new(w, h))
                .min_size(Vec2::new(380.0, 220.0))
                .resizable(true);
            win = apply_tile_rect(win, self.pending_tiles.remove(&WinKey::Processes));
            let mut next_id = self.next_aux_req_id;
            let resp = win.show(&ctx, |ui| {
                self.processes.show(
                    ui,
                    &mut || {
                        let id = next_id;
                        next_id += 1;
                        id
                    },
                    &mut outgoing,
                );
            });
            self.next_aux_req_id = next_id;
            if let Some(r) = resp {
                self.window_layers.insert(WinKey::Processes, r.response.layer_id);
                if r.response.clicked() {
                    self.active_window = Some(WinKey::Processes);
                    self.focused_term = None;
                }
            }
            self.processes.open = open;
        }

        // 8. Floating syntax-styled File Viewer windows (constrained to workspace_rect).
        let mut closed_viewers = Vec::new();
        let mut next_id = self.next_aux_req_id;
        for (idx, viewer) in self.viewers.iter_mut().enumerate() {
            let key = WinKey::Viewer(viewer.id);
            let mut open = viewer.open;
            let is_active = self.active_window == Some(key);
            let offset = (idx as f32 % 6.0) * 24.0;
            let w = (workspace_rect.width() * 0.72).clamp(400.0, 780.0);
            let h = (workspace_rect.height() * 0.74).clamp(280.0, 580.0);
            let default_pos = Pos2::new(
                (workspace_rect.left() + 28.0 + offset).min(workspace_rect.right() - 200.0),
                (workspace_rect.top() + 24.0 + offset).min(workspace_rect.bottom() - 160.0),
            );
            let title = format!("[VIEW] {}", viewer.display_name);
            let frame = window_frame(&ctx, is_active);
            let mut win = Window::new(title)
                .id(egui::Id::unique(("win_viewer", viewer.id)))
                .open(&mut open)
                .frame(frame)
                .constrain_to(workspace_rect)
                .default_pos(default_pos)
                .default_size(Vec2::new(w, h))
                .min_size(Vec2::new(300.0, 180.0))
                .resizable(true);
            win = apply_tile_rect(win, self.pending_tiles.remove(&key));
            let resp = win.show(&ctx, |ui| {
                viewer.show(
                    ui,
                    &mut || {
                        let id = next_id;
                        next_id += 1;
                        id
                    },
                    &mut outgoing,
                );
            });
            if let Some(r) = resp {
                self.window_layers.insert(key, r.response.layer_id);
                if r.response.clicked() {
                    self.active_window = Some(key);
                    self.focused_term = None;
                }
            }
            viewer.open = open;
            if !open {
                closed_viewers.push(viewer.id);
            }
        }
        self.next_aux_req_id = next_id;
        for id in closed_viewers {
            self.viewers.retain(|v| v.id != id);
            self.window_layers.remove(&WinKey::Viewer(id));
        }

        // 9. Floating remote Terminal windows (initial terminal centered proportionally in workspace_rect).
        let mut closed_terms = Vec::new();
        for (idx, term) in self.terminals.iter_mut().enumerate() {
            let term_id = term.id;
            let key = WinKey::Terminal(term_id);
            let mut open = term.open;
            let is_focused = self.focused_term == Some(term_id);
            let title = format!("[TERM] {}", term.title);
            let frame = window_frame(&ctx, is_focused).inner_margin(Margin::same(4));

            let (default_pos, default_size) = if idx == 0 {
                // Center the initial terminal cleanly inside CentralPanel with proportional margins.
                let pad_x = (workspace_rect.width() * 0.03).clamp(12.0, 28.0);
                let pad_y = (workspace_rect.height() * 0.03).clamp(12.0, 28.0);
                let w = (workspace_rect.width() - pad_x * 2.0 - 12.0).max(320.0);
                let h = (workspace_rect.height() - pad_y * 2.0 - 34.0).max(200.0);
                (Pos2::new(workspace_rect.left() + pad_x, workspace_rect.top() + pad_y), Vec2::new(w, h))
            } else {
                let offset = (idx as f32 % 6.0) * 26.0;
                let w = (workspace_rect.width() * 0.80).clamp(360.0, 860.0);
                let h = (workspace_rect.height() * 0.76).clamp(220.0, 560.0);
                let pos = Pos2::new(
                    (workspace_rect.left() + 20.0 + offset).min(workspace_rect.right() - 220.0),
                    (workspace_rect.top() + 20.0 + offset).min(workspace_rect.bottom() - 160.0),
                );
                (pos, Vec2::new(w, h))
            };

            let mut win = Window::new(title)
                .id(egui::Id::unique(("term_win", term_id)))
                .open(&mut open)
                .frame(frame)
                .constrain_to(workspace_rect)
                .default_pos(default_pos)
                .default_size(default_size)
                .min_size(Vec2::new(260.0, 140.0))
                .resizable(true);
            win = apply_tile_rect(win, self.pending_tiles.remove(&key));

            let resp = win.show(&ctx, |ui| show_terminal(ui, term, is_focused, &mut outgoing));

            if let Some(r) = &resp {
                self.window_layers.insert(key, r.response.layer_id);
            }
            if resp.as_ref().and_then(|r| r.inner).unwrap_or(false)
                || resp.as_ref().is_some_and(|r| r.response.clicked())
            {
                self.focused_term = Some(term_id);
                self.active_window = Some(key);
            }
            term.open = open;
            if !open {
                closed_terms.push(term_id);
            }
        }

        for id in closed_terms {
            self.terminals.retain(|t| t.id != id);
            self.window_layers.remove(&WinKey::Terminal(id));
            outgoing.push(ClientMsg::CloseTerminal { term_id: id });
            if self.focused_term == Some(id) {
                self.focused_term = self.terminals.last().map(|t| t.id);
                self.active_window = self.focused_term.map(WinKey::Terminal);
            }
        }

        for msg in outgoing {
            self.send(msg);
        }
    }
}

fn apply_tile_rect<'a>(win: Window<'a>, tile: Option<Rect>) -> Window<'a> {
    if let Some(r) = tile {
        win.current_pos(r.min).min_size(r.size()).max_size(r.size())
    } else {
        win
    }
}

fn window_frame(ctx: &egui::Context, focused: bool) -> Frame {
    Frame::window(&ctx.global_style())
        .fill(theme::BG_PANEL)
        .stroke(Stroke::new(1.0, if focused { theme::BORDER_FOCUS } else { theme::BORDER }))
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

fn truncate_label(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(2)).collect();
        out.push_str("..");
        out
    }
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
