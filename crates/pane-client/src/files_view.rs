//! Remote File Explorer state, chunked download receiver (saving to
//! `~/Downloads`), text-preview trigger, and egui renderer.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use egui::{Color32, RichText, ScrollArea};
use pane_proto::{ClientMsg, DirEntry};

use crate::metrics::fmt_bytes;
use crate::theme;

pub struct ActiveDownload {
    pub filename: String,
    pub dest_path: PathBuf,
    pub file: File,
    pub received: u64,
    pub total: u64,
}

pub struct CompletedDownload {
    pub filename: String,
    pub dest_path: PathBuf,
    pub bytes: u64,
}

pub struct FileExplorer {
    pub open: bool,
    pub docked: bool,
    pub current_path: String,
    pub path_input: String,
    pub entries: Vec<DirEntry>,
    pub filter: String,
    pub show_hidden: bool,
    pub loading: bool,
    pub last_error: Option<String>,
    pub active_downloads: HashMap<u64, ActiveDownload>,
    pub recent_downloads: Vec<CompletedDownload>,
    next_req_id: u64,
}

impl FileExplorer {
    pub fn new() -> Self {
        Self {
            open: true,
            docked: true,
            current_path: "~".into(),
            path_input: "~".into(),
            entries: Vec::new(),
            filter: String::new(),
            show_hidden: false,
            loading: false,
            last_error: None,
            active_downloads: HashMap::new(),
            recent_downloads: Vec::new(),
            next_req_id: 1,
        }
    }

    fn alloc_req_id(&mut self) -> u64 {
        let id = self.next_req_id;
        self.next_req_id += 1;
        id
    }

    pub fn request_dir(&mut self, path: impl Into<String>, out: &mut Vec<ClientMsg>) {
        let path = path.into();
        let req_id = self.alloc_req_id();
        self.loading = true;
        self.last_error = None;
        out.push(ClientMsg::ListDir { req_id, path });
    }

    pub fn on_listing(&mut self, path: String, entries: Vec<DirEntry>) {
        self.current_path = path.clone();
        self.path_input = path;
        self.entries = entries;
        self.loading = false;
        self.last_error = None;
    }

    pub fn start_download(&mut self, entry: &DirEntry, out: &mut Vec<ClientMsg>) {
        let downloads_dir = dirs::download_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
            .unwrap_or_else(|| PathBuf::from("."));

        if let Err(e) = fs::create_dir_all(&downloads_dir) {
            self.last_error = Some(format!("cannot create {}: {e}", downloads_dir.display()));
            return;
        }

        let dest_path = unique_dest_path(&downloads_dir, &entry.name);
        let file = match File::create(&dest_path) {
            Ok(f) => f,
            Err(e) => {
                self.last_error = Some(format!("cannot create {}: {e}", dest_path.display()));
                return;
            }
        };

        let remote_path = join_remote(&self.current_path, &entry.name);
        let req_id = self.alloc_req_id();
        self.active_downloads.insert(
            req_id,
            ActiveDownload {
                filename: entry.name.clone(),
                dest_path,
                file,
                received: 0,
                total: entry.size,
            },
        );
        self.last_error = None;
        out.push(ClientMsg::DownloadFile { req_id, path: remote_path });
    }

    pub fn on_chunk(&mut self, req_id: u64, total_size: u64, data: &[u8], eof: bool) -> Option<String> {
        let Some(dl) = self.active_downloads.get_mut(&req_id) else { return None };
        if let Err(e) = dl.file.write_all(data) {
            let err = format!("write {} failed: {e}", dl.dest_path.display());
            self.last_error = Some(err.clone());
            self.active_downloads.remove(&req_id);
            return Some(err);
        }
        dl.received += data.len() as u64;
        dl.total = total_size.max(dl.received);
        if eof {
            let done = self.active_downloads.remove(&req_id).unwrap();
            let msg = format!(
                "Saved {} ({}) -> {}",
                done.filename,
                fmt_bytes(done.received as f64),
                done.dest_path.display()
            );
            self.recent_downloads.insert(
                0,
                CompletedDownload { filename: done.filename, dest_path: done.dest_path, bytes: done.received },
            );
            self.recent_downloads.truncate(4);
            return Some(msg);
        }
        None
    }

    pub fn on_error(&mut self, req_id: Option<u64>, message: String) {
        self.loading = false;
        if let Some(id) = req_id {
            self.active_downloads.remove(&id);
        }
        self.last_error = Some(message);
    }

    /// Render the File Explorer. Returns `Some(remote_path)` if the user
    /// clicked a file to preview in the syntax-styled File Viewer.
    pub fn show(&mut self, ui: &mut egui::Ui, out: &mut Vec<ClientMsg>) -> Option<String> {
        let mut preview_target: Option<String> = None;

        // Navigation toolbar (pure ASCII labels to avoid missing glyph boxes).
        ui.horizontal(|ui| {
            if ui.button("Up").on_hover_text("Parent directory").clicked() {
                let parent = parent_remote(&self.current_path);
                self.request_dir(parent, out);
            }
            if ui.button("~").on_hover_text("Home directory").clicked() {
                self.request_dir("~", out);
            }
            if ui.button("Refresh").on_hover_text("Reload listing").clicked() {
                let cur = self.current_path.clone();
                self.request_dir(cur, out);
            }
            let dock_label = if self.docked { "Float" } else { "Dock" };
            if ui.button(dock_label).clicked() {
                self.docked = !self.docked;
            }
        });

        // Editable path bar.
        ui.horizontal(|ui| {
            let resp = ui.add_sized(
                [ui.available_width() - 36.0, 22.0],
                egui::TextEdit::singleline(&mut self.path_input).font(egui::TextStyle::Monospace),
            );
            let go = ui.button("Go").clicked()
                || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if go {
                let p = self.path_input.clone();
                self.request_dir(p, out);
            }
        });

        // Filter & hidden toggle.
        ui.horizontal(|ui| {
            ui.add_sized(
                [ui.available_width() - 78.0, 20.0],
                egui::TextEdit::singleline(&mut self.filter).hint_text("Filter..."),
            );
            ui.checkbox(&mut self.show_hidden, "Hidden");
        });

        if let Some(err) = &self.last_error {
            ui.label(RichText::new(err).color(theme::ACCENT_CORAL).small());
        }

        // Active download progress bars.
        for dl in self.active_downloads.values() {
            let frac = if dl.total == 0 { 1.0 } else { (dl.received as f32 / dl.total as f32).clamp(0.0, 1.0) };
            ui.add(
                egui::ProgressBar::new(frac)
                    .text(format!(
                        "[DL] {} ({}/{})",
                        dl.filename,
                        fmt_bytes(dl.received as f64),
                        fmt_bytes(dl.total as f64)
                    ))
                    .desired_height(16.0),
            );
        }

        ui.separator();

        // Directory listing.
        let filter_lc = self.filter.to_lowercase();
        let mut nav_target: Option<String> = None;
        let mut dl_entry: Option<DirEntry> = None;

        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if self.loading && self.entries.is_empty() {
                ui.label(RichText::new("Loading...").color(theme::TEXT_MUTED));
                return;
            }
            for entry in &self.entries {
                if !self.show_hidden && entry.name.starts_with('.') {
                    continue;
                }
                if !filter_lc.is_empty() && !entry.name.to_lowercase().contains(&filter_lc) {
                    continue;
                }
                ui.horizontal(|ui| {
                    let (tag, color) = if entry.is_dir {
                        ("[DIR]", theme::ACCENT_CYAN)
                    } else if entry.is_symlink {
                        ("[LNK]", theme::ACCENT_VIOLET)
                    } else {
                        ("     ", theme::TEXT_MUTED)
                    };
                    ui.label(RichText::new(tag).monospace().size(10.5).color(color));

                    let name_text = RichText::new(&entry.name)
                        .monospace()
                        .size(12.0)
                        .color(if entry.is_dir { theme::TEXT_PRIMARY } else { Color32::from_rgb(196, 206, 220) });

                    if entry.is_dir {
                        if ui.selectable_label(false, name_text).clicked() {
                            nav_target = Some(join_remote(&self.current_path, &entry.name));
                        }
                    } else {
                        if ui
                            .selectable_label(false, name_text)
                            .on_hover_text("Click to inspect in File Viewer")
                            .clicked()
                        {
                            preview_target = Some(join_remote(&self.current_path, &entry.name));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button("[DL]")
                                .on_hover_text("Download to ~/Downloads")
                                .clicked()
                            {
                                dl_entry = Some(entry.clone());
                            }
                            ui.label(
                                RichText::new(fmt_bytes(entry.size as f64))
                                    .monospace()
                                    .size(10.5)
                                    .color(theme::TEXT_MUTED),
                            );
                        });
                    }
                });
            }
        });

        if let Some(dir) = nav_target {
            self.request_dir(dir, out);
        }
        if let Some(entry) = dl_entry {
            self.start_download(&entry, out);
        }

        if !self.recent_downloads.is_empty() {
            ui.separator();
            ui.label(RichText::new("Recent downloads (~/Downloads)").small().color(theme::TEXT_MUTED));
            for d in &self.recent_downloads {
                ui.label(
                    RichText::new(format!("[OK] {} ({})", d.filename, fmt_bytes(d.bytes as f64)))
                        .monospace()
                        .size(11.0)
                        .color(theme::ACCENT_GREEN),
                )
                .on_hover_text(d.dest_path.display().to_string());
            }
        }

        preview_target
    }
}

fn join_remote(base: &str, name: &str) -> String {
    if base == "/" { format!("/{name}") } else { format!("{}/{name}", base.trim_end_matches('/')) }
}

fn parent_remote(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => trimmed[..i].to_string(),
    }
}

fn unique_dest_path(dir: &Path, filename: &str) -> PathBuf {
    let safe = Path::new(filename)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    let candidate = dir.join(&safe);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(&safe).file_stem().and_then(|s| s.to_str()).unwrap_or(&safe);
    let ext = Path::new(&safe).extension().and_then(|s| s.to_str());
    for i in 1..1000 {
        let name = match ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let p = dir.join(name);
        if !p.exists() {
            return p;
        }
    }
    candidate
}
