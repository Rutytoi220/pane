//! Floating syntax-styled remote File Viewer & Editable Text/Code Editor.
//!
//! Renders up to 256 KiB of remote UTF-8 text/config files with line numbers
//! and language-aware syntax coloring in Preview mode, or toggles into Edit
//! mode to modify and save changes straight back to the remote host (`Ctrl+S`).

use std::path::Path;

use egui::text::LayoutJob;
use egui::{Color32, FontId, Key, RichText, ScrollArea, TextFormat, TextWrapMode};
use pane_proto::ClientMsg;

use crate::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Yaml,
    Toml,
    Json,
    Python,
    Rust,
    Shell,
    Plain,
}

impl Lang {
    fn from_path(path: &str) -> Self {
        let name = Path::new(path).file_name().and_then(|s| s.to_str()).unwrap_or("");
        let ext = Path::new(path).extension().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase();
        if name.eq_ignore_ascii_case("Dockerfile") || name.ends_with(".env") || name == ".env" {
            return Self::Shell;
        }
        match ext.as_str() {
            "yml" | "yaml" => Self::Yaml,
            "toml" | "ini" | "conf" | "cfg" | "service" => Self::Toml,
            "json" | "jsonc" => Self::Json,
            "py" | "pyi" => Self::Python,
            "rs" => Self::Rust,
            "sh" | "bash" | "zsh" | "env" => Self::Shell,
            _ => Self::Plain,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Yaml => "YAML",
            Self::Toml => "CONFIG",
            Self::Json => "JSON",
            Self::Python => "PYTHON",
            Self::Rust => "RUST",
            Self::Shell => "SHELL",
            Self::Plain => "TEXT",
        }
    }
}

pub struct FileViewerPane {
    pub id: u64,
    pub workspace: u8,
    pub path: String,
    pub display_name: String,
    pub lines: Vec<String>,
    pub raw_content: String,
    pub truncated: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub open: bool,
    pub req_id: u64,
    pub edit_mode: bool,
    pub dirty: bool,
    pub saving: bool,
    pub save_req_id: Option<u64>,
    pub save_status: Option<(String, bool)>,
    lang: Lang,
}

impl FileViewerPane {
    pub fn new(id: u64, workspace: u8, req_id: u64, path: String) -> Self {
        let display_name = Path::new(&path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&path)
            .to_owned();
        let lang = Lang::from_path(&path);
        Self {
            id,
            workspace,
            path,
            display_name,
            lines: Vec::new(),
            raw_content: String::new(),
            truncated: false,
            loading: true,
            error: None,
            open: true,
            req_id,
            edit_mode: false,
            dirty: false,
            saving: false,
            save_req_id: None,
            save_status: None,
            lang,
        }
    }

    pub fn set_content(&mut self, path: String, content: String, truncated: bool) {
        self.path = path;
        self.lang = Lang::from_path(&self.path);
        self.lines = split_lines(&content);
        self.raw_content = content;
        self.truncated = truncated;
        self.loading = false;
        self.dirty = false;
        self.error = None;
    }

    pub fn set_error(&mut self, message: String) {
        self.loading = false;
        self.error = Some(message);
    }

    pub fn on_save_result(&mut self, ok: bool, error: Option<String>) {
        self.saving = false;
        self.save_req_id = None;
        if ok {
            self.dirty = false;
            self.lines = split_lines(&self.raw_content);
            self.save_status = Some(("Saved".into(), true));
        } else {
            self.save_status = Some((error.unwrap_or_else(|| "Save failed".into()), false));
        }
    }

    fn trigger_save(&mut self, alloc_req_id: &mut impl FnMut() -> u64, out: &mut Vec<ClientMsg>) {
        if self.saving || self.truncated {
            return;
        }
        let req_id = alloc_req_id();
        self.saving = true;
        self.save_req_id = Some(req_id);
        self.save_status = Some(("Saving...".into(), true));
        out.push(ClientMsg::WriteTextFile { req_id, path: self.path.clone(), content: self.raw_content.clone() });
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        is_active: bool,
        alloc_req_id: &mut impl FnMut() -> u64,
        out: &mut Vec<ClientMsg>,
    ) {
        // Ctrl+S shortcut when this editor window is active in Edit mode.
        if is_active
            && self.edit_mode
            && !self.loading
            && !self.truncated
            && ui.input(|i| (i.modifiers.ctrl || i.modifiers.command) && i.key_pressed(Key::S))
        {
            self.trigger_save(alloc_req_id, out);
        }

        // Header bar: language badge, remote path, Edit/Preview toggle, Save, Copy, Refresh.
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("[{}]", self.lang.label()))
                    .monospace()
                    .size(11.0)
                    .color(theme::ACCENT_CYAN),
            );
            ui.add(
                egui::Label::new(RichText::new(&self.path).monospace().size(11.0).color(theme::TEXT_MUTED))
                    .wrap_mode(TextWrapMode::Truncate),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Refresh").clicked() {
                    let req_id = alloc_req_id();
                    self.req_id = req_id;
                    self.loading = true;
                    self.error = None;
                    self.save_status = None;
                    out.push(ClientMsg::ReadTextFile { req_id, path: self.path.clone() });
                }
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(self.raw_content.clone());
                }
                if self.edit_mode {
                    let save_label = if self.saving {
                        "Saving..."
                    } else if self.dirty {
                        "Save* (Ctrl+S)"
                    } else {
                        "Save (Ctrl+S)"
                    };
                    let save_col = if self.dirty { theme::ACCENT_GREEN } else { theme::TEXT_PRIMARY };
                    if ui.add_enabled(!self.saving, egui::Button::new(RichText::new(save_label).color(save_col)).small()).clicked() {
                        self.trigger_save(alloc_req_id, out);
                    }
                    if ui.small_button("Preview").clicked() {
                        self.lines = split_lines(&self.raw_content);
                        self.edit_mode = false;
                    }
                } else {
                    let edit_btn = ui
                        .add_enabled(!self.truncated && !self.loading, egui::Button::new("Edit").small())
                        .on_disabled_hover_text("File exceeds 256 KB preview limit; download to edit");
                    if edit_btn.clicked() {
                        self.edit_mode = true;
                        self.save_status = None;
                    }
                }
            });
        });

        if self.truncated {
            ui.label(
                RichText::new("Notice: preview truncated to first 256 KB (use [DL] for the full file)")
                    .small()
                    .color(theme::ACCENT_AMBER),
            );
        }
        if let Some((msg, ok)) = &self.save_status {
            let col = if *ok { theme::ACCENT_GREEN } else { theme::ACCENT_CORAL };
            ui.label(RichText::new(msg).small().color(col));
        }

        ui.separator();

        if self.loading {
            ui.label(RichText::new("Loading file...").monospace().color(theme::TEXT_MUTED));
            return;
        }
        if let Some(err) = &self.error {
            ui.label(RichText::new(err).monospace().color(theme::ACCENT_CORAL));
            return;
        }

        if self.edit_mode {
            egui::Frame::new()
                .fill(theme::BG_TERM)
                .stroke(egui::Stroke::new(1.0, theme::BORDER_FOCUS))
                .corner_radius(3.0)
                .inner_margin(6.0)
                .show(ui, |ui| {
                    ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                        let avail = ui.available_size();
                        let resp = ui.add_sized(
                            avail,
                            egui::TextEdit::multiline(&mut self.raw_content)
                                .code_editor()
                                .desired_width(f32::INFINITY)
                                .lock_focus(true),
                        );
                        if resp.changed() {
                            self.dirty = true;
                            self.save_status = None;
                        }
                    });
                });
            return;
        }

        let font = FontId::monospace(12.5);
        let row_h = ui.fonts_mut(|f| f.row_height(&font).max(16.0));
        let total_lines = self.lines.len();
        let gutter_digits = total_lines.max(1).to_string().len().max(3);

        egui::Frame::new()
            .fill(theme::BG_TERM)
            .stroke(egui::Stroke::new(1.0, theme::BORDER))
            .corner_radius(3.0)
            .inner_margin(6.0)
            .show(ui, |ui| {
                ScrollArea::both().auto_shrink([false, false]).show_rows(ui, row_h, total_lines, |ui, range| {
                    for idx in range {
                        let line = &self.lines[idx];
                        let job = highlight_line(idx + 1, gutter_digits, line, self.lang, &font);
                        ui.add(egui::Label::new(job).extend());
                    }
                });
            });
    }
}

fn split_lines(content: &str) -> Vec<String> {
    let mut lines: Vec<String> = content.lines().map(str::to_owned).collect();
    if lines.is_empty() && !content.is_empty() {
        lines.push(content.to_owned());
    }
    lines
}

fn highlight_line(line_no: usize, gutter_w: usize, line: &str, lang: Lang, font: &FontId) -> LayoutJob {
    let mut job = LayoutJob::default();
    let push = |job: &mut LayoutJob, text: &str, color: Color32| {
        if !text.is_empty() {
            job.append(text, 0.0, TextFormat { font_id: font.clone(), color, ..Default::default() });
        }
    };

    push(&mut job, &format!("{line_no:>gutter_w$} | "), Color32::from_rgb(76, 90, 114));

    let trimmed = line.trim_start();
    let is_hash_comment = matches!(lang, Lang::Yaml | Lang::Toml | Lang::Python | Lang::Shell) && trimmed.starts_with('#');
    let is_slash_comment = matches!(lang, Lang::Rust | Lang::Json) && trimmed.starts_with("//");
    if is_hash_comment || is_slash_comment {
        push(&mut job, line, Color32::from_rgb(100, 118, 142));
        return job;
    }

    if lang == Lang::Toml && trimmed.starts_with('[') && trimmed.ends_with(']') {
        push(&mut job, line, theme::ACCENT_VIOLET);
        return job;
    }

    if matches!(lang, Lang::Yaml | Lang::Toml | Lang::Json) {
        let sep = if lang == Lang::Toml { '=' } else { ':' };
        if let Some(pos) = find_unquoted_sep(line, sep) {
            let (key_part, rest) = line.split_at(pos);
            push(&mut job, key_part, theme::ACCENT_CYAN);
            push(&mut job, &rest[..1], theme::TEXT_MUTED);
            highlight_tokens(&mut job, &rest[1..], lang, font);
            return job;
        }
    }

    highlight_tokens(&mut job, line, lang, font);
    job
}

fn find_unquoted_sep(s: &str, sep: char) -> Option<usize> {
    let mut in_single = false;
    let mut in_double = false;
    for (i, ch) in s.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => return None,
            c if c == sep && !in_single && !in_double => return Some(i),
            _ => {}
        }
    }
    None
}

fn highlight_tokens(job: &mut LayoutJob, s: &str, lang: Lang, font: &FontId) {
    let push = |job: &mut LayoutJob, text: &str, color: Color32| {
        if !text.is_empty() {
            job.append(text, 0.0, TextFormat { font_id: font.clone(), color, ..Default::default() });
        }
    };

    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'#' && matches!(lang, Lang::Yaml | Lang::Toml | Lang::Python | Lang::Shell) {
            push(job, &s[i..], Color32::from_rgb(100, 118, 142));
            return;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') && matches!(lang, Lang::Rust | Lang::Json) {
            push(job, &s[i..], Color32::from_rgb(100, 118, 142));
            return;
        }

        if b == b'"' || b == b'\'' {
            let quote = b;
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            push(job, &s[start..i], theme::ACCENT_GREEN);
            continue;
        }

        if b.is_ascii_alphanumeric() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'.' | b'-')) {
                i += 1;
            }
            let tok = &s[start..i];
            let color = if is_keyword(tok, lang) {
                theme::ACCENT_VIOLET
            } else if matches!(tok, "true" | "false" | "True" | "False" | "null" | "None") {
                theme::ACCENT_AMBER
            } else if tok.chars().all(|c| c.is_ascii_digit() || c == '.') && tok.chars().any(|c| c.is_ascii_digit()) {
                theme::ACCENT_AMBER
            } else {
                theme::TEXT_PRIMARY
            };
            push(job, tok, color);
            continue;
        }

        let ch_len = s[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        push(job, &s[i..i + ch_len], theme::TEXT_PRIMARY);
        i += ch_len;
    }
}

fn is_keyword(tok: &str, lang: Lang) -> bool {
    match lang {
        Lang::Python => matches!(
            tok,
            "def" | "class" | "import" | "from" | "as" | "return" | "if" | "elif" | "else"
                | "for" | "while" | "with" | "try" | "except" | "finally" | "async" | "await"
                | "in" | "not" | "and" | "or" | "pass" | "raise" | "yield" | "lambda"
        ),
        Lang::Rust => matches!(
            tok,
            "fn" | "pub" | "struct" | "enum" | "impl" | "trait" | "let" | "mut" | "const"
                | "use" | "mod" | "if" | "else" | "match" | "for" | "while" | "loop" | "return"
                | "async" | "await" | "where" | "type" | "self" | "Self" | "crate"
        ),
        Lang::Shell => matches!(
            tok,
            "if" | "then" | "else" | "fi" | "for" | "do" | "done" | "while" | "case" | "esac"
                | "export" | "local" | "FROM" | "RUN" | "COPY" | "WORKDIR" | "CMD" | "ENTRYPOINT" | "ENV" | "EXPOSE"
        ),
        _ => false,
    }
}
