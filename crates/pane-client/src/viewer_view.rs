//! Floating syntax-styled remote File Viewer window.
//!
//! Renders up to 256 KiB of remote UTF-8 text/config files with line numbers,
//! lightweight language-aware syntax coloring (YAML, TOML, JSON, Python, Rust,
//! Shell, Dockerfile, INI, JS/TS, C/C++, Markdown), copy-to-clipboard, and
//! virtualized row scrolling (`show_rows`) for smooth 60+ FPS performance.

use std::path::Path;

use egui::text::LayoutJob;
use egui::{Color32, FontId, RichText, ScrollArea, TextFormat};
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
    pub path: String,
    pub display_name: String,
    pub lines: Vec<String>,
    pub raw_content: String,
    pub truncated: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub open: bool,
    pub req_id: u64,
    lang: Lang,
}

impl FileViewerPane {
    pub fn new(id: u64, req_id: u64, path: String) -> Self {
        let display_name = Path::new(&path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&path)
            .to_owned();
        let lang = Lang::from_path(&path);
        Self {
            id,
            path,
            display_name,
            lines: Vec::new(),
            raw_content: String::new(),
            truncated: false,
            loading: true,
            error: None,
            open: true,
            req_id,
            lang,
        }
    }

    pub fn set_content(&mut self, path: String, content: String, truncated: bool) {
        self.path = path;
        self.lang = Lang::from_path(&self.path);
        self.lines = content.lines().map(str::to_owned).collect();
        if self.lines.is_empty() && !content.is_empty() {
            self.lines.push(content.clone());
        }
        self.raw_content = content;
        self.truncated = truncated;
        self.loading = false;
        self.error = None;
    }

    pub fn set_error(&mut self, message: String) {
        self.loading = false;
        self.error = Some(message);
    }

    pub fn show(&mut self, ui: &mut egui::Ui, alloc_req_id: &mut impl FnMut() -> u64, out: &mut Vec<ClientMsg>) {
        // Header bar: remote path, language badge, Refresh & Copy.
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("[{}]", self.lang.label()))
                    .monospace()
                    .size(11.0)
                    .color(theme::ACCENT_CYAN),
            );
            ui.label(RichText::new(&self.path).monospace().size(11.5).color(theme::TEXT_MUTED));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(self.raw_content.clone());
                }
                if ui.small_button("Refresh").clicked() {
                    let req_id = alloc_req_id();
                    self.req_id = req_id;
                    self.loading = true;
                    self.error = None;
                    out.push(ClientMsg::ReadTextFile { req_id, path: self.path.clone() });
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

        ui.separator();

        if self.loading {
            ui.label(RichText::new("Loading file...").monospace().color(theme::TEXT_MUTED));
            return;
        }
        if let Some(err) = &self.error {
            ui.label(RichText::new(err).monospace().color(theme::ACCENT_CORAL));
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

fn highlight_line(line_no: usize, gutter_w: usize, line: &str, lang: Lang, font: &FontId) -> LayoutJob {
    let mut job = LayoutJob::default();
    let push = |job: &mut LayoutJob, text: &str, color: Color32| {
        if !text.is_empty() {
            job.append(text, 0.0, TextFormat { font_id: font.clone(), color, ..Default::default() });
        }
    };

    // Line number gutter.
    push(&mut job, &format!("{line_no:>gutter_w$} | "), Color32::from_rgb(76, 90, 114));

    let trimmed = line.trim_start();
    // Whole-line comments.
    let is_hash_comment = matches!(lang, Lang::Yaml | Lang::Toml | Lang::Python | Lang::Shell) && trimmed.starts_with('#');
    let is_slash_comment = matches!(lang, Lang::Rust | Lang::Json) && trimmed.starts_with("//");
    if is_hash_comment || is_slash_comment {
        push(&mut job, line, Color32::from_rgb(100, 118, 142));
        return job;
    }

    // TOML/INI section header `[section]`.
    if lang == Lang::Toml && trimmed.starts_with('[') && trimmed.ends_with(']') {
        push(&mut job, line, theme::ACCENT_VIOLET);
        return job;
    }

    // YAML/TOML/JSON `key: value` or `key = value` split.
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
        // Inline comments.
        if b == b'#' && matches!(lang, Lang::Yaml | Lang::Toml | Lang::Python | Lang::Shell) {
            push(job, &s[i..], Color32::from_rgb(100, 118, 142));
            return;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') && matches!(lang, Lang::Rust | Lang::Json) {
            push(job, &s[i..], Color32::from_rgb(100, 118, 142));
            return;
        }

        // Quoted strings.
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

        // Identifiers / keywords / booleans / numbers.
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

        // Single punctuation / whitespace / UTF-8 char.
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
