//! Quick App Launcher command palette (`Ctrl+Space` or `Launcher` dock button)
//! plus SSH GUI app bridge (`waypipe ssh` with automatic `ssh -Y` fallback).

use std::process::{Command, Stdio};
use std::thread;

use egui::{Align2, Frame, Key, Margin, Order, Rect, RichText, Stroke, Vec2};

use crate::theme;
use crate::transport::LaunchMode;

#[derive(Debug, Clone)]
pub enum LauncherAction {
    NewTerminal,
    ToggleFiles,
    OpenProcesses,
    ToggleMonitor,
    OpenServices,
    TileWindows,
    /// Attach to the remote host's live X11/Xwayland desktop (`:0` / `:1`).
    AttachLiveDesktop,
    /// Launch a remote X11 GUI app seamlessly inside `pane` via live X11 or Xvfb.
    LaunchEmbeddedGui(String),
    /// Launch a remote GUI app externally via `waypipe ssh` / `ssh -Y`.
    RunExternalGui(String),
}

struct NativeAppItem {
    name: &'static str,
    tag: &'static str,
    desc: &'static str,
    action: LauncherAction,
}

const NATIVE_APPS: &[NativeAppItem] = &[
    NativeAppItem {
        name: "Terminal",
        tag: "[TERM]",
        desc: "Open a new remote PTY terminal in the current workspace",
        action: LauncherAction::NewTerminal,
    },
    NativeAppItem {
        name: "File Explorer",
        tag: "[FILES]",
        desc: "Toggle the remote File Explorer panel",
        action: LauncherAction::ToggleFiles,
    },
    NativeAppItem {
        name: "Process Manager",
        tag: "[PROC]",
        desc: "Open live /proc process table with CPU/RAM sorting and Kill",
        action: LauncherAction::OpenProcesses,
    },
    NativeAppItem {
        name: "Service & Docker Manager",
        tag: "[SVC]",
        desc: "Inspect and restart/stop remote systemd units and Docker containers",
        action: LauncherAction::OpenServices,
    },
    NativeAppItem {
        name: "System Monitor",
        tag: "[MON]",
        desc: "Toggle the live CPU, RAM, and VNC bandwidth-savings monitor",
        action: LauncherAction::ToggleMonitor,
    },
    NativeAppItem {
        name: "Stream Live Desktop (:0)",
        tag: "[DESK]",
        desc: "Attach to the remote host's live X11/Xwayland desktop (:0/:1) inside pane",
        action: LauncherAction::AttachLiveDesktop,
    },
    NativeAppItem {
        name: "Tile Windows",
        tag: "[TILE]",
        desc: "Arrange all open windows in the current workspace into a grid",
        action: LauncherAction::TileWindows,
    },
];

const GUI_PRESETS: &[(&str, &str)] = &[
    ("firefox", "Launch remote Firefox"),
    ("dolphin", "Launch remote KDE Dolphin file manager"),
    ("kcalc", "Launch remote KCalc calculator"),
    ("konsole --nofork", "Launch remote KDE Konsole terminal"),
    ("thunar", "Launch remote Thunar file manager"),
];

pub struct AppLauncher {
    pub open: bool,
    pub query: String,
    pub use_external_waypipe: bool,
    just_opened: bool,
}

impl AppLauncher {
    pub fn new() -> Self {
        Self { open: false, query: String::new(), use_external_waypipe: false, just_opened: false }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.query.clear();
            self.just_opened = true;
        }
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// Render the centered command palette overlay inside `workspace_rect`.
    pub fn show(&mut self, ctx: &egui::Context, workspace_rect: Rect) -> Option<LauncherAction> {
        if !self.open {
            return None;
        }

        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.close();
            return None;
        }

        let mut chosen: Option<LauncherAction> = None;
        let width = (workspace_rect.width() * 0.62).clamp(380.0, 580.0);
        let center = workspace_rect.center();

        egui::Area::new(egui::Id::unique("pane_app_launcher"))
            .order(Order::Foreground)
            .pivot(Align2::CENTER_CENTER)
            .fixed_pos(center)
            .show(ctx, |ui| {
                Frame::new()
                    .fill(theme::BG_HUD)
                    .stroke(Stroke::new(1.5, theme::ACCENT_CYAN))
                    .corner_radius(6.0)
                    .inner_margin(Margin::same(14))
                    .show(ui, |ui| {
                        ui.set_width(width);

                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("APP LAUNCHER & SSH GUI BRIDGE")
                                    .monospace()
                                    .strong()
                                    .size(12.0)
                                    .color(theme::ACCENT_CYAN),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("Esc").clicked() {
                                    self.open = false;
                                }
                            });
                        });

                        ui.add_space(6.0);

                        let input_resp = ui.add_sized(
                            Vec2::new(width, 28.0),
                            egui::TextEdit::singleline(&mut self.query)
                                .hint_text("Search pane apps or type a remote GUI command (e.g. firefox, thunar)...")
                                .font(egui::TextStyle::Monospace),
                        );
                        if self.just_opened {
                            input_resp.request_focus();
                            self.just_opened = false;
                        }

                        let enter_pressed = ui.input(|i| i.key_pressed(Key::Enter));
                        let q = self.query.trim();
                        let q_lc = q.to_lowercase();

                        ui.add_space(8.0);
                        ui.label(RichText::new("NATIVE PANE APPS").small().strong().color(theme::TEXT_MUTED));

                        let mut first_match: Option<LauncherAction> = None;
                        for item in NATIVE_APPS {
                            if !q_lc.is_empty()
                                && !item.name.to_lowercase().contains(&q_lc)
                                && !item.tag.to_lowercase().contains(&q_lc)
                                && !item.desc.to_lowercase().contains(&q_lc)
                            {
                                continue;
                            }
                            if first_match.is_none() {
                                first_match = Some(item.action.clone());
                            }
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(item.tag).monospace().size(11.0).color(theme::ACCENT_CYAN));
                                if ui
                                    .selectable_label(false, RichText::new(item.name).strong().color(theme::TEXT_PRIMARY))
                                    .clicked()
                                {
                                    chosen = Some(item.action.clone());
                                }
                                ui.label(RichText::new(item.desc).small().color(theme::TEXT_MUTED));
                            });
                        }

                        ui.add_space(8.0);
                        ui.separator();
                        ui.horizontal(|ui| {
                            let mode_label = if self.use_external_waypipe {
                                "REMOTE GUI APPS (external waypipe / ssh -Y)"
                            } else {
                                "REMOTE GUI APPS (native seamless inside pane via Xvfb)"
                            };
                            ui.label(RichText::new(mode_label).small().strong().color(theme::ACCENT_GREEN));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.checkbox(&mut self.use_external_waypipe, "External waypipe");
                            });
                        });

                        let make_gui_action = |cmd: String, external: bool| {
                            if external {
                                LauncherAction::RunExternalGui(cmd)
                            } else {
                                LauncherAction::LaunchEmbeddedGui(cmd)
                            }
                        };

                        if !q.is_empty() {
                            let cmd_text = if self.use_external_waypipe {
                                format!("[WAYPIPE] Run `{q}` externally")
                            } else {
                                format!("[APP] Launch `{q}` inside pane")
                            };
                            if ui
                                .button(RichText::new(cmd_text).monospace().color(theme::ACCENT_GREEN))
                                .clicked()
                            {
                                chosen = Some(make_gui_action(q.to_owned(), self.use_external_waypipe));
                            }
                            if first_match.is_none() {
                                first_match = Some(make_gui_action(q.to_owned(), self.use_external_waypipe));
                            }
                        } else {
                            ui.horizontal_wrapped(|ui| {
                                for &(cmd, desc) in GUI_PRESETS {
                                    if ui
                                        .button(RichText::new(format!("[APP] {cmd}")).monospace())
                                        .on_hover_text(desc)
                                        .clicked()
                                    {
                                        chosen = Some(make_gui_action(cmd.to_owned(), self.use_external_waypipe));
                                    }
                                }
                            });
                        }

                        if enter_pressed && chosen.is_none() {
                            chosen = first_match;
                        }
                    });
            });

        if chosen.is_some() {
            self.close();
        }
        chosen
    }
}

/// Spawn a remote graphical application over SSH using `waypipe ssh <target> <cmd>`,
/// automatically falling back to `ssh -Y <target> <cmd>` if `waypipe` is missing
/// locally or on the remote host.
pub fn spawn_remote_gui_app(mode: &LaunchMode, cmd_line: String) -> String {
    let cmd_trimmed = cmd_line.trim().to_owned();
    if cmd_trimmed.is_empty() {
        return "No GUI command provided".into();
    }

    match mode.clone() {
        LaunchMode::Ssh { target, ssh_opts, .. } => {
            let notice = format!("Launching remote GUI `{cmd_trimmed}` on {target} (waypipe / ssh -Y)...");
            thread::Builder::new()
                .name("pane-gui-bridge".into())
                .spawn(move || {
                    let has_local_waypipe = Command::new("waypipe")
                        .arg("--version")
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success());

                    let has_remote_waypipe = if has_local_waypipe {
                        let mut probe = Command::new("ssh");
                        probe.arg("-T");
                        for opt in &ssh_opts {
                            probe.args(["-o", opt]);
                        }
                        probe
                            .arg(&target)
                            .arg("command -v waypipe >/dev/null 2>&1")
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status()
                            .is_ok_and(|s| s.success())
                    } else {
                        false
                    };

                    if has_local_waypipe && has_remote_waypipe {
                        eprintln!("[pane-gui] launching `{cmd_trimmed}` via waypipe ssh {target}");
                        let mut c = Command::new("waypipe");
                        c.arg("ssh");
                        for opt in &ssh_opts {
                            c.args(["-o", opt]);
                        }
                        c.arg(&target).arg(&cmd_trimmed);
                        if let Err(e) = c.spawn() {
                            eprintln!("[pane-gui] waypipe failed ({e}), falling back to ssh -Y");
                            spawn_ssh_x11(&target, &ssh_opts, &cmd_trimmed);
                        }
                    } else {
                        eprintln!(
                            "[pane-gui] waypipe unavailable (local={has_local_waypipe}, remote={has_remote_waypipe}); \
                             falling back to ssh -Y {target} {cmd_trimmed}"
                        );
                        spawn_ssh_x11(&target, &ssh_opts, &cmd_trimmed);
                    }
                })
                .ok();
            notice
        }
        LaunchMode::Demo { .. } => {
            let notice = format!("Launching local demo GUI `{cmd_trimmed}`...");
            let _ = Command::new("sh").args(["-c", &cmd_trimmed]).spawn();
            notice
        }
    }
}

fn spawn_ssh_x11(target: &str, ssh_opts: &[String], cmd_line: &str) {
    let mut c = Command::new("ssh");
    c.arg("-Y");
    for opt in ssh_opts {
        c.args(["-o", opt]);
    }
    c.arg(target).arg(cmd_line);
    if let Err(e) = c.spawn() {
        eprintln!("[pane-gui] ssh -Y failed: {e}");
    }
}
