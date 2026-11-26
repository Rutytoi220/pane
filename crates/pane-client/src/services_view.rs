//! Floating Service & Docker Container Manager (`[SVC]`).

use egui::{Color32, RichText, ScrollArea, TextWrapMode};
use pane_proto::{ClientMsg, ContainerInfo, ServiceAction, ServiceInfo};

use crate::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SvcTab {
    Systemd,
    Docker,
}

pub struct ServiceManager {
    pub open: bool,
    pub workspace: u8,
    pub services: Vec<ServiceInfo>,
    pub containers: Vec<ContainerInfo>,
    pub systemd_available: bool,
    pub docker_available: bool,
    pub loading: bool,
    pub filter: String,
    pub last_status: Option<(String, bool)>,
    tab: SvcTab,
}

impl ServiceManager {
    pub fn new() -> Self {
        Self {
            open: false,
            workspace: 1,
            services: Vec::new(),
            containers: Vec::new(),
            systemd_available: true,
            docker_available: false,
            loading: false,
            filter: String::new(),
            last_status: None,
            tab: SvcTab::Systemd,
        }
    }

    pub fn request_refresh(&mut self, req_id: u64, out: &mut Vec<ClientMsg>) {
        self.loading = true;
        out.push(ClientMsg::ListServices { req_id });
    }

    pub fn on_snapshot(
        &mut self,
        services: Vec<ServiceInfo>,
        containers: Vec<ContainerInfo>,
        systemd_available: bool,
        docker_available: bool,
    ) {
        self.services = services;
        self.containers = containers;
        self.systemd_available = systemd_available;
        self.docker_available = docker_available;
        self.loading = false;
    }

    pub fn on_control_result(&mut self, ok: bool, message: String) {
        self.last_status = Some((message, ok));
    }

    pub fn show(&mut self, ui: &mut egui::Ui, alloc_req_id: &mut impl FnMut() -> u64, out: &mut Vec<ClientMsg>) {
        // Tab switcher + filter + Refresh.
        ui.horizontal(|ui| {
            let sys_label = format!("Systemd ({})", self.services.len());
            if ui.selectable_label(self.tab == SvcTab::Systemd, sys_label).clicked() {
                self.tab = SvcTab::Systemd;
            }
            let dock_label = format!("Docker ({})", self.containers.len());
            if ui.selectable_label(self.tab == SvcTab::Docker, dock_label).clicked() {
                self.tab = SvcTab::Docker;
            }
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .desired_width(150.0)
                    .hint_text("Filter..."),
            );
            if ui.button("Refresh").clicked() {
                let req_id = alloc_req_id();
                self.request_refresh(req_id, out);
            }
        });

        if let Some((msg, ok)) = &self.last_status {
            let col = if *ok { theme::ACCENT_GREEN } else { theme::ACCENT_CORAL };
            ui.label(RichText::new(msg).small().color(col));
        }

        ui.separator();

        if self.loading && self.services.is_empty() && self.containers.is_empty() {
            ui.label(RichText::new("Querying remote services & containers...").monospace().color(theme::TEXT_MUTED));
            return;
        }

        let filter_lc = self.filter.to_lowercase();
        let mut svc_action: Option<(String, ServiceAction)> = None;
        let mut ctr_action: Option<(String, ServiceAction)> = None;

        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
            SvcTab::Systemd => {
                if !self.systemd_available {
                    ui.label(RichText::new("systemctl is not available on this host.").color(theme::TEXT_MUTED));
                    return;
                }
                egui::Grid::new("systemd_grid")
                    .num_columns(4)
                    .striped(true)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("Unit").strong().color(theme::TEXT_MUTED));
                        ui.label(RichText::new("State").strong().color(theme::TEXT_MUTED));
                        ui.label(RichText::new("Description").strong().color(theme::TEXT_MUTED));
                        ui.label(RichText::new("Control").strong().color(theme::TEXT_MUTED));
                        ui.end_row();

                        for s in &self.services {
                            if !filter_lc.is_empty()
                                && !s.name.to_lowercase().contains(&filter_lc)
                                && !s.description.to_lowercase().contains(&filter_lc)
                            {
                                continue;
                            }
                            ui.label(RichText::new(&s.name).monospace().size(11.5).color(theme::TEXT_PRIMARY));
                            let (state_txt, state_col) = match (s.active_state.as_str(), s.sub_state.as_str()) {
                                ("active", "running") => ("running", theme::ACCENT_GREEN),
                                ("active", sub) => (sub, theme::ACCENT_CYAN),
                                ("failed", _) => ("failed", theme::ACCENT_CORAL),
                                (act, _) => (act, theme::TEXT_MUTED),
                            };
                            ui.label(RichText::new(state_txt).monospace().size(11.0).color(state_col));
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&s.description).size(11.5).color(Color32::from_rgb(170, 182, 200)),
                                )
                                .wrap_mode(TextWrapMode::Truncate),
                            );
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                if ui.small_button("Restart").clicked() {
                                    svc_action = Some((s.name.clone(), ServiceAction::Restart));
                                }
                                if s.active_state == "active" {
                                    if ui
                                        .small_button(RichText::new("Stop").color(theme::ACCENT_CORAL))
                                        .clicked()
                                    {
                                        svc_action = Some((s.name.clone(), ServiceAction::Stop));
                                    }
                                } else if ui
                                    .small_button(RichText::new("Start").color(theme::ACCENT_GREEN))
                                    .clicked()
                                {
                                    svc_action = Some((s.name.clone(), ServiceAction::Start));
                                }
                            });
                            ui.end_row();
                        }
                    });
            }
            SvcTab::Docker => {
                if !self.docker_available {
                    ui.label(
                        RichText::new("Docker is not installed or the daemon is not accessible by this user.")
                            .color(theme::TEXT_MUTED),
                    );
                    return;
                }
                if self.containers.is_empty() {
                    ui.label(RichText::new("No Docker containers found.").color(theme::TEXT_MUTED));
                    return;
                }
                egui::Grid::new("docker_grid")
                    .num_columns(5)
                    .striped(true)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("Name").strong().color(theme::TEXT_MUTED));
                        ui.label(RichText::new("Image").strong().color(theme::TEXT_MUTED));
                        ui.label(RichText::new("State").strong().color(theme::TEXT_MUTED));
                        ui.label(RichText::new("Status").strong().color(theme::TEXT_MUTED));
                        ui.label(RichText::new("Control").strong().color(theme::TEXT_MUTED));
                        ui.end_row();

                        for c in &self.containers {
                            if !filter_lc.is_empty()
                                && !c.name.to_lowercase().contains(&filter_lc)
                                && !c.image.to_lowercase().contains(&filter_lc)
                            {
                                continue;
                            }
                            ui.label(RichText::new(&c.name).monospace().size(11.5).color(theme::TEXT_PRIMARY));
                            ui.label(RichText::new(&c.image).monospace().size(11.0).color(theme::ACCENT_CYAN));
                            let col = match c.state.as_str() {
                                "running" => theme::ACCENT_GREEN,
                                "restarting" => theme::ACCENT_AMBER,
                                "exited" | "dead" => theme::ACCENT_CORAL,
                                _ => theme::TEXT_MUTED,
                            };
                            ui.label(RichText::new(&c.state).monospace().size(11.0).color(col));
                            ui.label(RichText::new(&c.status).size(11.0).color(theme::TEXT_MUTED));
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                if ui.small_button("Restart").clicked() {
                                    ctr_action = Some((c.id.clone(), ServiceAction::Restart));
                                }
                                if c.state == "running" {
                                    if ui
                                        .small_button(RichText::new("Stop").color(theme::ACCENT_CORAL))
                                        .clicked()
                                    {
                                        ctr_action = Some((c.id.clone(), ServiceAction::Stop));
                                    }
                                } else if ui
                                    .small_button(RichText::new("Start").color(theme::ACCENT_GREEN))
                                    .clicked()
                                {
                                    ctr_action = Some((c.id.clone(), ServiceAction::Start));
                                }
                            });
                            ui.end_row();
                        }
                    });
            }
        });

        if let Some((name, action)) = svc_action {
            let req_id = alloc_req_id();
            self.last_status = Some((format!("Running systemctl {} {name}...", action.as_str()), true));
            out.push(ClientMsg::ControlService { req_id, name, action });
        }
        if let Some((id, action)) = ctr_action {
            let req_id = alloc_req_id();
            self.last_status = Some((format!("Running docker {} {id}...", action.as_str()), true));
            out.push(ClientMsg::ControlContainer { req_id, id, action });
        }
    }
}
