//! Root egui desktop environment:
//! - Top HUD bar: brand, virtual workspaces (`1`, `2`, `3`), host info, live
//!   RTT ping, compressed KB/s rate, total bytes, and VNC bandwidth savings.
//! - Docked/floating File Explorer (left) & System Monitor (right), clamped to
//!   <= 32% viewport width each.
//! - Constrained `CentralPanel` workspace for `[TERM]`, `[VIEW]`, `[PROC]`, and
//!   `[SVC]` windows with deterministic `tile_epoch`-based grid tiling.
//! - Centered Quick App Launcher (`Ctrl+Space`) with SSH GUI bridge (`waypipe`
//!   -> `ssh -Y` fallback).
//! - Bottom Desktop Dock / Taskbar with quick-launchers, `Tile Windows`, and
//!   one-click window switcher tabs for the active workspace.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use egui::{CentralPanel, Color32, Frame, Key, LayerId, Margin, Panel, Pos2, Rect, RichText, Sense, Stroke, Vec2, Window};
use pane_proto::{ClientMsg, HostMsg, PROTOCOL_VERSION};

use crate::files_view::FileExplorer;
use crate::launcher::{AppLauncher, LauncherAction, spawn_remote_gui_app};
use crate::metrics::{SessionMetrics, fmt_bytes};
use crate::monitor_view::{HostInfo, SystemMonitor, TelemetryState};
use crate::processes_view::ProcessManager;
use crate::services_view::ServiceManager;
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
    Services,
    Files,
    Monitor,
}

pub struct PaneApp {
    mode: LaunchMode,
    transport: Option<Transport>,
    host: Option<HostInfo>,
    telemetry: TelemetryState,
    metrics: SessionMetrics,

    /// Active virtual desktop workspace (`1`, `2`, or `3`).
    active_workspace: u8,
    /// Monotonic tiling counter; incremented on initial layout & `Tile Windows`.
    tile_epoch: u64,
    /// Last `tile_epoch` applied to each floating window.
    last_applied_tile_epoch: HashMap<WinKey, u64>,

    terminals: Vec<TerminalPane>,
    next_term_id: u32,
    focused_term: Option<u32>,

    viewers: Vec<FileViewerPane>,
    next_viewer_id: u64,
    next_aux_req_id: u64,

    files: FileExplorer,
    files_workspace: u8,
    monitor: SystemMonitor,
    monitor_workspace: u8,
    processes: ProcessManager,
    services: ServiceManager,
    launcher: AppLauncher,

    /// Workspace rect captured from `CentralPanel` each frame.
    workspace_rect: Rect,
    /// True once the first `CentralPanel` pass has measured the real workspace rect.
    layout_ready: bool,
    /// Active floating window key (highlighted in taskbar).
    active_window: Option<WinKey>,
    /// Last rendered `LayerId` per floating window so taskbar clicks can raise it.
    window_layers: HashMap<WinKey, LayerId>,

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
            active_workspace: 1,
            tile_epoch: 1,
            last_applied_tile_epoch: HashMap::new(),
            terminals: Vec::new(),
            next_term_id: 1,
            focused_term: None,
            viewers: Vec::new(),
            next_viewer_id: 1,
            next_aux_req_id: 100_000,
            files: FileExplorer::new(),
            files_workspace: 1,
            monitor: SystemMonitor::new(),
            monitor_workspace: 1,
            processes: ProcessManager::new(),
            services: ServiceManager::new(),
            launcher: AppLauncher::new(),
            workspace_rect: Rect::from_min_size(Pos2::new(270.0, 36.0), Vec2::new(880.0, 780.0)),
            layout_ready: false,
            active_window: None,
            window_layers: HashMap::new(),
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

    fn switch_workspace(&mut self, ws: u8) {
        let ws = ws.clamp(1, 3);
        if self.active_workspace == ws {
            return;
        }
        self.active_workspace = ws;
        // Focus the most recent terminal in the target workspace, if any.
        self.focused_term = self.terminals.iter().rev().find(|t| t.open && t.workspace == ws).map(|t| t.id);
        self.active_window = self.focused_term.map(WinKey::Terminal);
    }

    fn workspace_window_count(&self, ws: u8) -> usize {
        let mut n = self.terminals.iter().filter(|t| t.open && t.workspace == ws).count();
        n += self.viewers.iter().filter(|v| v.open && v.workspace == ws).count();
        if self.processes.open && self.processes.workspace == ws {
            n += 1;
        }
        if self.services.open && self.services.workspace == ws {
            n += 1;
        }
        if self.files.open && !self.files.docked && self.files_workspace == ws {
            n += 1;
        }
        if self.monitor.open && !self.monitor.docked && self.monitor_workspace == ws {
            n += 1;
        }
        n
    }

    fn active_floating_keys(&self) -> Vec<WinKey> {
        let ws = self.active_workspace;
        let mut keys = Vec::new();
        for t in &self.terminals {
            if t.open && t.workspace == ws {
                keys.push(WinKey::Terminal(t.id));
            }
        }
        for v in &self.viewers {
            if v.open && v.workspace == ws {
                keys.push(WinKey::Viewer(v.id));
            }
        }
        if self.processes.open && self.processes.workspace == ws {
            keys.push(WinKey::Processes);
        }
        if self.services.open && self.services.workspace == ws {
            keys.push(WinKey::Services);
        }
        if self.files.open && !self.files.docked && self.files_workspace == ws {
            keys.push(WinKey::Files);
        }
        if self.monitor.open && !self.monitor.docked && self.monitor_workspace == ws {
            keys.push(WinKey::Monitor);
        }
        keys
    }

    fn trigger_tile(&mut self) {
        self.tile_epoch = self.tile_epoch.wrapping_add(1).max(1);
    }

    fn spawn_terminal(&mut self) {
        let id = self.next_term_id;
        self.next_term_id += 1;
        let (cols, rows) = (96, 28);
        self.terminals.push(TerminalPane::new(id, self.active_workspace, cols, rows));
        self.focused_term = Some(id);
        self.active_window = Some(WinKey::Terminal(id));
        self.send(ClientMsg::OpenTerminal { term_id: id, cols, rows });
    }

    fn open_file_viewer(&mut self, ctx: &egui::Context, path: String, out: &mut Vec<ClientMsg>) {
        if let Some(existing) = self.viewers.iter_mut().find(|v| v.path == path) {
            existing.open = true;
            existing.workspace = self.active_workspace;
            let key = WinKey::Viewer(existing.id);
            self.raise_window(ctx, key);
            return;
        }
        let id = self.next_viewer_id;
        self.next_viewer_id += 1;
        let req_id = self.alloc_aux_req_id();
        self.viewers.push(FileViewerPane::new(id, self.active_workspace, req_id, path.clone()));
        self.active_window = Some(WinKey::Viewer(id));
        self.focused_term = None;
        out.push(ClientMsg::ReadTextFile { req_id, path });
    }

    fn open_processes(&mut self, ctx: &egui::Context, out: &mut Vec<ClientMsg>) {
        self.processes.open = true;
        self.processes.workspace = self.active_workspace;
        let req_id = self.alloc_aux_req_id();
        self.processes.request_refresh(req_id, out);
        self.raise_window(ctx, WinKey::Processes);
    }

    fn open_services(&mut self, ctx: &egui::Context, out: &mut Vec<ClientMsg>) {
        self.services.open = true;
        self.services.workspace = self.active_workspace;
        let req_id = self.alloc_aux_req_id();
        self.services.request_refresh(req_id, out);
        self.raise_window(ctx, WinKey::Services);
    }

    fn raise_window(&mut self, ctx: &egui::Context, key: WinKey) {
        self.active_window = Some(key);
        if let WinKey::Terminal(id) = key {
            self.focused_term = Some(id);
            if let Some(t) = self.terminals.iter_mut().find(|t| t.id == id) {
                t.want_focus = true;
            }
        } else {
            self.focused_term = None;
        }
        if let Some(&layer_id) = self.window_layers.get(&key) {
            ctx.move_to_top(layer_id);
        }
    }

    fn notify(&mut self, msg: String, color: Color32) {
        self.status_banner = Some((msg, color, Instant::now()));
    }

    fn handle_launcher_action(&mut self, ctx: &egui::Context, action: LauncherAction, out: &mut Vec<ClientMsg>) {
        match action {
            LauncherAction::NewTerminal => self.spawn_terminal(),
            LauncherAction::ToggleFiles => {
                self.files.open = !self.files.open;
                if self.files.open && !self.files.docked {
                    self.files_workspace = self.active_workspace;
                    self.raise_window(ctx, WinKey::Files);
                }
            }
            LauncherAction::OpenProcesses => self.open_processes(ctx, out),
            LauncherAction::ToggleMonitor => {
                self.monitor.open = !self.monitor.open;
                if self.monitor.open && !self.monitor.docked {
                    self.monitor_workspace = self.active_workspace;
                    self.raise_window(ctx, WinKey::Monitor);
                }
            }
            LauncherAction::OpenServices => self.open_services(ctx, out),
            LauncherAction::TileWindows => self.trigger_tile(),
            LauncherAction::RunRemoteGui(cmd) => {
                let notice = spawn_remote_gui_app(&self.mode, cmd);
                self.notify(notice, theme::ACCENT_GREEN);
            }
        }
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
                    self.terminals.push(TerminalPane::new(id, 1, 96, 28));
                    self.focused_term = Some(id);
                    self.active_window = Some(WinKey::Terminal(id));
                    // Trigger initial tile so Terminal #1 fills CentralPanel cleanly.
                    self.trigger_tile();
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
            HostMsg::WriteFileResult { req_id, path, ok, error } => {
                if let Some(v) = self.viewers.iter_mut().find(|v| v.save_req_id == Some(req_id) || v.path == path) {
                    v.on_save_result(ok, error.clone());
                }
                if ok {
                    self.notify(format!("Saved {path}"), theme::ACCENT_GREEN);
                } else {
                    self.notify(error.unwrap_or_else(|| format!("Failed to save {path}")), theme::ACCENT_CORAL);
                }
            }
            HostMsg::ProcessList { processes, .. } => self.processes.on_list(processes),
            HostMsg::ServiceSnapshot { services, containers, systemd_available, docker_available, .. } => {
                self.services.on_snapshot(services, containers, systemd_available, docker_available);
            }
            HostMsg::ControlResult { ok, message, .. } => {
                self.services.on_control_result(ok, message.clone());
                self.notify(message, if ok { theme::ACCENT_GREEN } else { theme::ACCENT_CORAL });
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

    fn handle_global_shortcuts(&mut self, ctx: &egui::Context) {
        let mut target_ws: Option<u8> = None;
        let mut toggle_launcher = false;

        ctx.input_mut(|i| {
            if i.modifiers.ctrl && !i.modifiers.alt && i.consume_key(egui::Modifiers::CTRL, Key::Space) {
                toggle_launcher = true;
            }
            if i.modifiers.alt && !i.modifiers.ctrl {
                if i.consume_key(egui::Modifiers::ALT, Key::Num1) {
                    target_ws = Some(1);
                } else if i.consume_key(egui::Modifiers::ALT, Key::Num2) {
                    target_ws = Some(2);
                } else if i.consume_key(egui::Modifiers::ALT, Key::Num3) {
                    target_ws = Some(3);
                }
            }
        });

        if toggle_launcher {
            self.launcher.toggle();
        }
        if let Some(ws) = target_ws {
            self.switch_workspace(ws);
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

                // Virtual Workspace pills (1, 2, 3) with Alt+1/2/3 hints.
                let mut clicked_ws: Option<u8> = None;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    for ws in 1..=3u8 {
                        let count = self.workspace_window_count(ws);
                        let active = self.active_workspace == ws;
                        let label = if count > 0 { format!("{ws}:{count}") } else { format!("{ws}") };
                        let color = if active {
                            theme::ACCENT_CYAN
                        } else if count > 0 {
                            theme::TEXT_PRIMARY
                        } else {
                            theme::TEXT_MUTED
                        };
                        if ui
                            .selectable_label(active, RichText::new(label).monospace().strong().size(11.5).color(color))
                            .on_hover_text(format!("Workspace {ws} (Alt+{ws})"))
                            .clicked()
                        {
                            clicked_ws = Some(ws);
                        }
                    }
                });
                if let Some(ws) = clicked_ws {
                    self.switch_workspace(ws);
                }

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
                // Quick App Launcher button (Ctrl+Space).
                if ui
                    .selectable_label(
                        self.launcher.open,
                        RichText::new("Launcher").color(theme::ACCENT_GREEN).strong(),
                    )
                    .on_hover_text("Open App Launcher & SSH GUI Bridge (Ctrl+Space)")
                    .clicked()
                {
                    self.launcher.toggle();
                }
                if ui.button(RichText::new("+ Terminal").color(theme::ACCENT_CYAN).strong()).clicked() {
                    self.spawn_terminal();
                }
                if ui.selectable_label(self.files.open, "Files").clicked() {
                    self.files.open = !self.files.open;
                    if self.files.open && !self.files.docked {
                        self.files_workspace = self.active_workspace;
                        self.raise_window(ctx, WinKey::Files);
                    }
                }
                if ui.selectable_label(self.processes.open, "Processes").clicked() {
                    if self.processes.open && self.processes.workspace == self.active_workspace {
                        self.processes.open = false;
                    } else {
                        self.open_processes(ctx, out);
                    }
                }
                if ui.selectable_label(self.services.open, "Services").clicked() {
                    if self.services.open && self.services.workspace == self.active_workspace {
                        self.services.open = false;
                    } else {
                        self.open_services(ctx, out);
                    }
                }
                if ui.selectable_label(self.monitor.open, "Monitor").clicked() {
                    self.monitor.open = !self.monitor.open;
                    if self.monitor.open && !self.monitor.docked {
                        self.monitor_workspace = self.active_workspace;
                        self.raise_window(ctx, WinKey::Monitor);
                    }
                }
                if ui
                    .button("Tile Windows")
                    .on_hover_text("Divide CentralPanel evenly across all open windows in this workspace")
                    .clicked()
                {
                    self.trigger_tile();
                }

                ui.separator();

                // Window switcher tabs for all open floating windows in the active workspace.
                let ws = self.active_workspace;
                let mut raise_target: Option<WinKey> = None;

                egui::ScrollArea::horizontal().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for t in &self.terminals {
                            if t.workspace != ws {
                                continue;
                            }
                            let key = WinKey::Terminal(t.id);
                            let selected = self.active_window == Some(key);
                            let label = format!("[TERM] {}", truncate_label(&t.title, 18));
                            if ui.selectable_label(selected, RichText::new(label).monospace().size(11.5)).clicked() {
                                raise_target = Some(key);
                            }
                        }
                        for v in &self.viewers {
                            if v.workspace != ws {
                                continue;
                            }
                            let key = WinKey::Viewer(v.id);
                            let selected = self.active_window == Some(key);
                            let dirty_mark = if v.dirty { "*" } else { "" };
                            let label = format!("[VIEW] {dirty_mark}{}", truncate_label(&v.display_name, 18));
                            if ui.selectable_label(selected, RichText::new(label).monospace().size(11.5)).clicked() {
                                raise_target = Some(key);
                            }
                        }
                        if self.processes.open && self.processes.workspace == ws {
                            let selected = self.active_window == Some(WinKey::Processes);
                            if ui
                                .selectable_label(selected, RichText::new("[PROC] Processes").monospace().size(11.5))
                                .clicked()
                            {
                                raise_target = Some(WinKey::Processes);
                            }
                        }
                        if self.services.open && self.services.workspace == ws {
                            let selected = self.active_window == Some(WinKey::Services);
                            if ui
                                .selectable_label(selected, RichText::new("[SVC] Services").monospace().size(11.5))
                                .clicked()
                            {
                                raise_target = Some(WinKey::Services);
                            }
                        }
                        if self.files.open && !self.files.docked && self.files_workspace == ws {
                            let selected = self.active_window == Some(WinKey::Files);
                            if ui
                                .selectable_label(selected, RichText::new("[FILES] Files").monospace().size(11.5))
                                .clicked()
                            {
                                raise_target = Some(WinKey::Files);
                            }
                        }
                        if self.monitor.open && !self.monitor.docked && self.monitor_workspace == ws {
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

    /// Apply tiling rect to `win` if this window's `last_applied_tile_epoch`
    /// lags behind `self.tile_epoch`. Uses `.current_pos(...)` + `.fixed_size(...)`
    /// on the tiling frame so the outer window snaps to the exact grid slot, and
    /// `.resizable(true)` on subsequent frames so the user stays free to resize.
    fn apply_tile_if_needed<'a>(
        &mut self,
        win: Window<'a>,
        key: WinKey,
        tile_rects: &HashMap<WinKey, Rect>,
    ) -> Window<'a> {
        let applied = self.last_applied_tile_epoch.get(&key).copied().unwrap_or(0);
        if applied < self.tile_epoch
            && let Some(&rect) = tile_rects.get(&key)
        {
            self.last_applied_tile_epoch.insert(key, self.tile_epoch);
            win.current_pos(rect.min).fixed_size(rect.size())
        } else {
            win.resizable(true)
        }
    }
}

impl eframe::App for PaneApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_transport();
        let ctx = ui.ctx().clone();
        self.handle_global_shortcuts(&ctx);

        // Keep HUD counters (VNC savings & RTT) ticking smoothly every 250 ms even when idle.
        ctx.request_repaint_after(Duration::from_millis(250));

        let mut outgoing = Vec::new();
        let mut preview_requests: Vec<String> = Vec::new();

        // 1. Top HUD bar & Bottom Taskbar reserved first so CentralPanel's rect excludes both.
        self.show_hud(ui);
        self.show_taskbar(ui, &ctx, &mut outgoing);

        // Clamp each docked side panel to at most 32% of total viewport width
        // so CentralPanel always retains at least 36% of the screen.
        let viewport_w = ui.available_width().max(640.0);
        let max_dock_w = (viewport_w * 0.32).max(200.0);

        // 2. Docked File Explorer (left).
        if self.files.open && self.files.docked {
            let min_w = 180.0_f32.min(max_dock_w);
            let def_w = 270.0_f32.clamp(min_w, max_dock_w);
            Panel::left("files_dock")
                .resizable(true)
                .default_size(def_w)
                .min_size(min_w)
                .max_size(max_dock_w)
                .frame(panel_frame())
                .show(ui, |ui| {
                    ui.set_max_width(ui.available_width());
                    ui.label(RichText::new("REMOTE FILES").small().strong().color(theme::TEXT_MUTED));
                    if let Some(path) = self.files.show(ui, &mut outgoing) {
                        preview_requests.push(path);
                    }
                });
            if !self.files.docked {
                self.files_workspace = self.active_workspace;
            }
        }

        // 3. Docked System Monitor (right).
        if self.monitor.open && self.monitor.docked {
            let min_w = 200.0_f32.min(max_dock_w);
            let def_w = 285.0_f32.clamp(min_w, max_dock_w);
            Panel::right("monitor_dock")
                .resizable(true)
                .default_size(def_w)
                .min_size(min_w)
                .max_size(max_dock_w)
                .frame(panel_frame())
                .show(ui, |ui| {
                    ui.set_max_width(ui.available_width());
                    ui.label(RichText::new("SYSTEM MONITOR").small().strong().color(theme::TEXT_MUTED));
                    self.monitor.show(ui, self.host.as_ref(), &self.telemetry, &self.metrics);
                });
            if !self.monitor.docked {
                self.monitor_workspace = self.active_workspace;
            }
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
                } else if self.workspace_window_count(self.active_workspace) == 0 && self.host.is_some() {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Workspace {} is empty  --  press Ctrl+Space for Launcher or click + Terminal",
                                self.active_workspace
                            ))
                            .monospace()
                            .color(theme::TEXT_MUTED),
                        );
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

        if !self.layout_ready {
            self.layout_ready = true;
            for path in preview_requests {
                self.open_file_viewer(&ctx, path, &mut outgoing);
            }
            for msg in outgoing {
                self.send(msg);
            }
            ctx.request_repaint();
            return;
        }

        for path in preview_requests {
            self.open_file_viewer(&ctx, path, &mut outgoing);
        }

        // Compute deterministic tile grid rects for all open floating windows in the active workspace.
        let active_keys = self.active_floating_keys();
        let tile_rects = compute_tile_rects(workspace_rect, &active_keys);
        let ws = self.active_workspace;

        // 5. Floating File Explorer window when undocked.
        if self.files.open && !self.files.docked && self.files_workspace == ws {
            let mut open = self.files.open;
            let default_pos = Pos2::new(workspace_rect.left() + 14.0, workspace_rect.top() + 14.0);
            let default_size = Vec2::new(320.0, (workspace_rect.height() - 60.0).clamp(260.0, 560.0));
            let win = Window::new("[FILES] Remote Files")
                .id(egui::Id::unique("win_files"))
                .open(&mut open)
                .constrain_to(workspace_rect)
                .default_pos(default_pos)
                .default_size(default_size)
                .min_size(Vec2::new(220.0, 180.0));
            let win = self.apply_tile_if_needed(win, WinKey::Files, &tile_rects);
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

        // 6. Floating System Monitor window when undocked.
        if self.monitor.open && !self.monitor.docked && self.monitor_workspace == ws {
            let mut open = self.monitor.open;
            let w = 310.0;
            let default_pos = Pos2::new(
                (workspace_rect.right() - w - 16.0).max(workspace_rect.left() + 12.0),
                workspace_rect.top() + 14.0,
            );
            let default_size = Vec2::new(w, (workspace_rect.height() - 60.0).clamp(280.0, 560.0));
            let win = Window::new("[MON] System Monitor")
                .id(egui::Id::unique("win_monitor"))
                .open(&mut open)
                .constrain_to(workspace_rect)
                .default_pos(default_pos)
                .default_size(default_size)
                .min_size(Vec2::new(240.0, 220.0));
            let win = self.apply_tile_if_needed(win, WinKey::Monitor, &tile_rects);
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

        // 7. Floating Live Process Manager window (`[PROC]`).
        if self.processes.open && self.processes.workspace == ws {
            let mut open = self.processes.open;
            let w = (workspace_rect.width() * 0.68).clamp(420.0, 680.0);
            let h = (workspace_rect.height() * 0.68).clamp(300.0, 520.0);
            let default_pos = Pos2::new(workspace_rect.center().x - w * 0.5, workspace_rect.center().y - h * 0.5);
            let is_active = self.active_window == Some(WinKey::Processes);
            let win = Window::new("[PROC] Processes")
                .id(egui::Id::unique("win_processes"))
                .open(&mut open)
                .frame(window_frame(&ctx, is_active))
                .constrain_to(workspace_rect)
                .default_pos(default_pos)
                .default_size(Vec2::new(w, h))
                .min_size(Vec2::new(340.0, 200.0));
            let win = self.apply_tile_if_needed(win, WinKey::Processes, &tile_rects);
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

        // 8. Floating Service & Docker Container Manager (`[SVC]`).
        if self.services.open && self.services.workspace == ws {
            let mut open = self.services.open;
            let w = (workspace_rect.width() * 0.72).clamp(460.0, 760.0);
            let h = (workspace_rect.height() * 0.68).clamp(300.0, 540.0);
            let default_pos = Pos2::new(workspace_rect.center().x - w * 0.5, workspace_rect.center().y - h * 0.5);
            let is_active = self.active_window == Some(WinKey::Services);
            let win = Window::new("[SVC] Services & Docker")
                .id(egui::Id::unique("win_services"))
                .open(&mut open)
                .frame(window_frame(&ctx, is_active))
                .constrain_to(workspace_rect)
                .default_pos(default_pos)
                .default_size(Vec2::new(w, h))
                .min_size(Vec2::new(360.0, 220.0));
            let win = self.apply_tile_if_needed(win, WinKey::Services, &tile_rects);
            let mut next_id = self.next_aux_req_id;
            let resp = win.show(&ctx, |ui| {
                self.services.show(
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
                self.window_layers.insert(WinKey::Services, r.response.layer_id);
                if r.response.clicked() {
                    self.active_window = Some(WinKey::Services);
                    self.focused_term = None;
                }
            }
            self.services.open = open;
        }

        // 9. Floating syntax-styled File Viewer & Editor windows (`[VIEW]`).
        let mut closed_viewers = Vec::new();
        let mut next_id = self.next_aux_req_id;
        for idx in 0..self.viewers.len() {
            if self.viewers[idx].workspace != ws {
                continue;
            }
            let viewer_id = self.viewers[idx].id;
            let key = WinKey::Viewer(viewer_id);
            let mut open = self.viewers[idx].open;
            let is_active = self.active_window == Some(key) && !self.launcher.open;
            let offset = (idx as f32 % 6.0) * 24.0;
            let w = (workspace_rect.width() * 0.72).clamp(380.0, 780.0);
            let h = (workspace_rect.height() * 0.74).clamp(260.0, 580.0);
            let default_pos = Pos2::new(
                (workspace_rect.left() + 24.0 + offset).min(workspace_rect.right() - 200.0),
                (workspace_rect.top() + 20.0 + offset).min(workspace_rect.bottom() - 160.0),
            );
            let dirty_mark = if self.viewers[idx].dirty { "*" } else { "" };
            let title = format!("[VIEW] {dirty_mark}{}", self.viewers[idx].display_name);
            let win = Window::new(title)
                .id(egui::Id::unique(("win_viewer", viewer_id)))
                .open(&mut open)
                .frame(window_frame(&ctx, is_active))
                .constrain_to(workspace_rect)
                .default_pos(default_pos)
                .default_size(Vec2::new(w, h))
                .min_size(Vec2::new(280.0, 180.0));
            let win = self.apply_tile_if_needed(win, key, &tile_rects);
            let resp = win.show(&ctx, |ui| {
                self.viewers[idx].show(
                    ui,
                    is_active,
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
                if r.response.clicked() || r.response.contains_pointer() && ctx.input(|i| i.pointer.any_pressed()) {
                    self.active_window = Some(key);
                    self.focused_term = None;
                }
            }
            self.viewers[idx].open = open;
            if !open {
                closed_viewers.push(viewer_id);
            }
        }
        self.next_aux_req_id = next_id;
        for id in closed_viewers {
            self.viewers.retain(|v| v.id != id);
            self.window_layers.remove(&WinKey::Viewer(id));
            self.last_applied_tile_epoch.remove(&WinKey::Viewer(id));
        }

        // 10. Floating remote Terminal windows (`[TERM]`).
        let mut closed_terms = Vec::new();
        for idx in 0..self.terminals.len() {
            if self.terminals[idx].workspace != ws {
                continue;
            }
            let term_id = self.terminals[idx].id;
            let key = WinKey::Terminal(term_id);
            let mut open = self.terminals[idx].open;
            let is_focused = self.focused_term == Some(term_id) && !self.launcher.open;
            let title = format!("[TERM] {}", self.terminals[idx].title);
            let frame = window_frame(&ctx, is_focused).inner_margin(Margin::same(4));

            let (default_pos, default_size) = if let Some(&tile) = tile_rects.get(&key) {
                (tile.min, tile.size())
            } else {
                let offset = (idx as f32 % 6.0) * 26.0;
                let w = (workspace_rect.width() * 0.80).clamp(340.0, 860.0);
                let h = (workspace_rect.height() * 0.76).clamp(220.0, 560.0);
                (
                    Pos2::new(
                        (workspace_rect.left() + 18.0 + offset).min(workspace_rect.right() - 200.0),
                        (workspace_rect.top() + 18.0 + offset).min(workspace_rect.bottom() - 160.0),
                    ),
                    Vec2::new(w, h),
                )
            };

            let win = Window::new(title)
                .id(egui::Id::unique(("term_win", term_id)))
                .open(&mut open)
                .frame(frame)
                .constrain_to(workspace_rect)
                .default_pos(default_pos)
                .default_size(default_size)
                .min_size(Vec2::new(240.0, 140.0));
            let win = self.apply_tile_if_needed(win, key, &tile_rects);

            let resp = win.show(&ctx, |ui| show_terminal(ui, &mut self.terminals[idx], is_focused, &mut outgoing));

            if let Some(r) = &resp {
                self.window_layers.insert(key, r.response.layer_id);
            }
            if resp.as_ref().and_then(|r| r.inner).unwrap_or(false)
                || resp.as_ref().is_some_and(|r| r.response.clicked())
            {
                self.focused_term = Some(term_id);
                self.active_window = Some(key);
            }
            self.terminals[idx].open = open;
            if !open {
                closed_terms.push(term_id);
            }
        }

        for id in closed_terms {
            self.terminals.retain(|t| t.id != id);
            self.window_layers.remove(&WinKey::Terminal(id));
            self.last_applied_tile_epoch.remove(&WinKey::Terminal(id));
            outgoing.push(ClientMsg::CloseTerminal { term_id: id });
            if self.focused_term == Some(id) {
                self.focused_term = self.terminals.iter().rev().find(|t| t.workspace == ws).map(|t| t.id);
                self.active_window = self.focused_term.map(WinKey::Terminal);
            }
        }

        // 11. Centered Quick App Launcher overlay (`Ctrl+Space`).
        if let Some(action) = self.launcher.show(&ctx, workspace_rect) {
            self.handle_launcher_action(&ctx, action, &mut outgoing);
        }

        for msg in outgoing {
            self.send(msg);
        }
    }
}

/// Divide `workspace_rect` evenly into a clean non-overlapping grid covering
/// the entire central canvas:
/// - 1 window:  100% x 100%
/// - 2 windows: 50% x 100% side-by-side
/// - 3 windows: top row 2 x (50% x 50%), bottom row 1 x (100% x 50%)
/// - 4 windows: 2x2 grid (50% x 50% each)
/// - N windows: `ceil(sqrt(N))` columns x `ceil(N / cols)` rows
fn compute_tile_rects(workspace_rect: Rect, keys: &[WinKey]) -> HashMap<WinKey, Rect> {
    let mut out = HashMap::new();
    let n = keys.len();
    if n == 0 {
        return out;
    }

    let pad = 8.0;
    let gap = 8.0;
    let area = workspace_rect.shrink(pad);
    if area.width() <= 40.0 || area.height() <= 40.0 {
        return out;
    }

    let (cols, rows) = match n {
        1 => (1, 1),
        2 => (2, 1),
        3 | 4 => (2, 2),
        _ => {
            let c = (n as f32).sqrt().ceil() as usize;
            (c, n.div_ceil(c))
        }
    };

    let cell_w = ((area.width() - gap * (cols - 1) as f32) / cols as f32).max(160.0);
    let cell_h = ((area.height() - gap * (rows - 1) as f32) / rows as f32).max(120.0);

    for (i, &key) in keys.iter().enumerate() {
        let c = i % cols;
        let r = i / cols;
        let x = area.left() + c as f32 * (cell_w + gap);
        let y = area.top() + r as f32 * (cell_h + gap);
        // Stretch the last tile across remaining columns on an uneven final row.
        let is_last_on_partial_row = r == rows - 1 && i == n - 1 && (n % cols) != 0;
        let w = if is_last_on_partial_row { (area.right() - x).max(cell_w) } else { cell_w };
        let h = if r == rows - 1 { (area.bottom() - y).max(cell_h) } else { cell_h };
        out.insert(key, Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h)));
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_layout_divides_central_panel_evenly() {
        let ws = Rect::from_min_size(Pos2::new(270.0, 36.0), Vec2::new(900.0, 800.0));

        // 2 windows -> 50% width x 100% height side-by-side.
        let k2 = [WinKey::Terminal(1), WinKey::Processes];
        let r2 = compute_tile_rects(ws, &k2);
        let a = r2[&k2[0]];
        let b = r2[&k2[1]];
        assert!((a.width() - b.width()).abs() < 1.0);
        assert!((a.height() - b.height()).abs() < 1.0);
        assert!(b.left() > a.right(), "2 tiled windows must sit side-by-side without overlap");
        assert!(a.height() > 750.0, "2 tiled windows should span full central height");

        // 4 windows -> exact 2x2 grid.
        let k4 = [WinKey::Terminal(1), WinKey::Terminal(2), WinKey::Processes, WinKey::Services];
        let r4 = compute_tile_rects(ws, &k4);
        assert_eq!(r4.len(), 4);
        let tl = r4[&k4[0]];
        let tr = r4[&k4[1]];
        let bl = r4[&k4[2]];
        let br = r4[&k4[3]];
        assert!(tr.left() > tl.right());
        assert!(bl.top() > tl.bottom());
        assert!((tl.width() - br.width()).abs() < 1.0);
        assert!((tl.height() - br.height()).abs() < 1.0);
    }
}
