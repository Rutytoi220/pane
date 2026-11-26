//! System Monitor window: live CPU/RAM area charts, per-core bars, load
//! averages, host metadata, and pane-vs-VNC bandwidth efficiency card.

use std::collections::VecDeque;

use egui::{Color32, Pos2, Rect, RichText, ScrollArea, Sense, Stroke, TextWrapMode, Vec2};

use crate::metrics::{SessionMetrics, VNC_1080P60_BYTES_PER_SEC, fmt_bytes, fmt_uptime};
use crate::theme;

#[derive(Debug, Default)]
pub struct HostInfo {
    pub hostname: String,
    pub os_info: String,
    pub cpu_cores: u32,
    pub total_mem_mb: u64,
}

#[derive(Debug, Default)]
pub struct TelemetryState {
    pub cpu_percent: f32,
    pub mem_used_mb: u64,
    pub load_avg: [f32; 3],
    pub per_core: Vec<f32>,
    pub uptime_secs: u64,
    pub received: bool,
}

pub struct SystemMonitor {
    pub open: bool,
    pub docked: bool,
}

impl SystemMonitor {
    pub fn new() -> Self {
        Self { open: true, docked: true }
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        host: Option<&HostInfo>,
        tel: &TelemetryState,
        metrics: &SessionMetrics,
    ) {
        // Bound inner content to the panel width so widgets never push the SidePanel wider.
        let panel_w = ui.available_width().max(160.0);
        ui.set_max_width(panel_w);
        ui.style_mut().wrap_mode = Some(TextWrapMode::Truncate);

        ui.allocate_ui_with_layout(
            Vec2::new(panel_w, 22.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                let dock_label = if self.docked { "Float" } else { "Dock" };
                if ui.small_button(dock_label).clicked() {
                    self.docked = !self.docked;
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if let Some(h) = host {
                        ui.add(
                            egui::Label::new(RichText::new(&h.hostname).strong().color(theme::TEXT_PRIMARY))
                                .wrap_mode(TextWrapMode::Truncate),
                        );
                        ui.label(RichText::new(format!("({}c)", h.cpu_cores)).small().color(theme::TEXT_MUTED));
                    } else {
                        ui.label(RichText::new("Connecting...").color(theme::TEXT_MUTED));
                    }
                });
            },
        );
        if let Some(h) = host {
            ui.add(
                egui::Label::new(RichText::new(&h.os_info).small().color(theme::TEXT_MUTED))
                    .wrap_mode(TextWrapMode::Truncate),
            )
            .on_hover_text(&h.os_info);
        }

        ui.separator();

        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let content_w = (ui.available_width() - 2.0).max(140.0);
            ui.set_max_width(content_w);

            // CPU aggregate + sparkline.
            ui.horizontal(|ui| {
                ui.label(RichText::new("CPU").strong().color(theme::ACCENT_CYAN));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("{:5.1}%", tel.cpu_percent)).monospace().strong());
                });
            });
            draw_sparkline(ui, &metrics.cpu_history, 100.0, theme::ACCENT_CYAN, content_w, 50.0);

            // Per-core bars adapted to the panel's current width.
            if !tel.per_core.is_empty() {
                ui.add_space(4.0);
                let cols = if content_w >= 340.0 && tel.per_core.len() > 8 { 4 } else { 2 };
                let cell_w = ((content_w - (cols as f32 - 1.0) * 8.0) / cols as f32).max(60.0);
                let bar_w = (cell_w - 30.0).clamp(24.0, 96.0);
                egui::Grid::new("per_core_grid")
                    .num_columns(cols)
                    .spacing([8.0, 3.0])
                    .show(ui, |ui| {
                        for (i, &pct) in tel.per_core.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.label(
                                    RichText::new(format!("C{i:02}"))
                                        .monospace()
                                        .size(10.5)
                                        .color(theme::TEXT_MUTED),
                                );
                                let (r, _) = ui.allocate_exact_size(Vec2::new(bar_w, 8.0), Sense::hover());
                                let p = ui.painter();
                                p.rect_filled(r, 2.0, theme::BG_CANVAS);
                                let fill_w = bar_w * (pct / 100.0).clamp(0.0, 1.0);
                                let col = if pct > 80.0 {
                                    theme::ACCENT_CORAL
                                } else if pct > 50.0 {
                                    theme::ACCENT_AMBER
                                } else {
                                    theme::ACCENT_CYAN
                                };
                                p.rect_filled(Rect::from_min_size(r.min, Vec2::new(fill_w, r.height())), 2.0, col);
                            });
                            if (i + 1) % cols == 0 {
                                ui.end_row();
                            }
                        }
                    });
            }

            ui.add_space(6.0);

            // Memory bar + sparkline.
            let total_mem = host.map(|h| h.total_mem_mb).unwrap_or(0);
            let mem_frac =
                if total_mem > 0 { (tel.mem_used_mb as f32 / total_mem as f32).clamp(0.0, 1.0) } else { 0.0 };
            ui.horizontal(|ui| {
                ui.label(RichText::new("RAM").strong().color(theme::ACCENT_VIOLET));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!(
                            "{:.1}/{:.1} GB ({:.0}%)",
                            tel.mem_used_mb as f32 / 1024.0,
                            total_mem as f32 / 1024.0,
                            mem_frac * 100.0
                        ))
                        .monospace()
                        .size(11.0),
                    );
                });
            });
            ui.add(
                egui::ProgressBar::new(mem_frac)
                    .desired_width(content_w)
                    .desired_height(8.0)
                    .fill(theme::ACCENT_VIOLET),
            );
            draw_sparkline(ui, &metrics.mem_history, 100.0, theme::ACCENT_VIOLET, content_w, 36.0);

            ui.add_space(6.0);

            // Load average & uptime.
            ui.horizontal(|ui| {
                ui.label(RichText::new("LOAD").small().color(theme::TEXT_MUTED));
                ui.label(
                    RichText::new(format!("{:.2} {:.2} {:.2}", tel.load_avg[0], tel.load_avg[1], tel.load_avg[2]))
                        .monospace()
                        .size(11.5),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if tel.received {
                        ui.label(
                            RichText::new(format!("up {}", fmt_uptime(tel.uptime_secs)))
                                .monospace()
                                .small()
                                .color(theme::TEXT_MUTED),
                        );
                    }
                });
            });

            ui.separator();

            // Protocol efficiency card: pane state diffs vs 1080p60 VNC.
            ui.label(RichText::new("WIRE EFFICIENCY vs. 1080p60 VNC").small().strong().color(theme::ACCENT_GREEN));
            let card_inner_w = (content_w - 18.0).max(100.0);
            egui::Frame::new()
                .fill(theme::BG_CANVAS)
                .stroke(Stroke::new(1.0, theme::BORDER))
                .corner_radius(4.0)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.set_max_width(card_inner_w);
                    let bw_max = metrics.bw_kbps_history.iter().copied().fold(10.0f32, f32::max);
                    draw_sparkline(ui, &metrics.bw_kbps_history, bw_max, theme::ACCENT_GREEN, card_inner_w, 34.0);
                    ui.add_space(4.0);
                    metric_row(ui, "pane live rate", format!("{:.1} KB/s", metrics.bandwidth_kb_per_sec()), theme::ACCENT_GREEN);
                    metric_row(
                        ui,
                        "VNC 1080p60",
                        format!("{:.1} MB/s", VNC_1080P60_BYTES_PER_SEC / 1e6),
                        theme::TEXT_MUTED,
                    );
                    metric_row(ui, "pane total", fmt_bytes(metrics.total_bytes() as f64), theme::TEXT_PRIMARY);
                    metric_row(ui, "VNC equiv.", fmt_bytes(metrics.vnc_equivalent_bytes()), theme::TEXT_MUTED);
                    metric_row(
                        ui,
                        "Saved",
                        format!("{} ({:.2}%)", fmt_bytes(metrics.bytes_saved_vs_vnc()), metrics.saved_percent_vs_vnc()),
                        theme::ACCENT_GREEN,
                    );
                    metric_row(
                        ui,
                        "Frames RX/TX",
                        format!("{}/{}", metrics.frames_rx, metrics.frames_tx),
                        theme::TEXT_MUTED,
                    );
                });
        });
    }
}

fn metric_row(ui: &mut egui::Ui, label: &str, value: String, value_color: Color32) {
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(RichText::new(label).small().color(theme::TEXT_MUTED)).wrap_mode(TextWrapMode::Truncate));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value).monospace().small().color(value_color));
        });
    });
}

fn draw_sparkline(ui: &mut egui::Ui, data: &VecDeque<f32>, max_y: f32, color: Color32, width: f32, height: f32) {
    let width = width.max(40.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 3.0, theme::BG_CANVAS);
    p.rect_stroke(rect, 3.0, Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);

    // Subtle horizontal 50% gridline.
    let mid_y = rect.center().y;
    p.line_segment(
        [Pos2::new(rect.left() + 1.0, mid_y), Pos2::new(rect.right() - 1.0, mid_y)],
        Stroke::new(0.5, Color32::from_rgb(26, 33, 46)),
    );

    if data.len() < 2 || max_y <= 0.0 {
        return;
    }

    let step = rect.width() / (data.len() - 1) as f32;
    let pts: Vec<Pos2> = data
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = rect.left() + i as f32 * step;
            let norm = (v / max_y).clamp(0.0, 1.0);
            let y = rect.bottom() - 2.0 - norm * (rect.height() - 4.0);
            Pos2::new(x, y)
        })
        .collect();

    // Soft area fill via vertical strips (fast & convex-safe).
    let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 36);
    for w in pts.windows(2) {
        let poly = vec![w[0], w[1], Pos2::new(w[1].x, rect.bottom() - 1.0), Pos2::new(w[0].x, rect.bottom() - 1.0)];
        p.add(egui::Shape::convex_polygon(poly, fill, Stroke::NONE));
    }
    p.add(egui::Shape::line(pts, Stroke::new(1.5, color)));
}
