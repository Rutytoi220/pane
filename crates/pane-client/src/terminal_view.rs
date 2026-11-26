//! GPU-rendered terminal viewport: draws retained `pane_proto::Screen` state
//! directly via egui's tessellator and converts keyboard/scroll/resize events
//! back into `ClientMsg` commands.

use egui::{Align2, Color32, Event, FontId, Key, Modifiers, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use pane_proto::{ClientMsg, Screen, StyleFlags};

use crate::theme;

const FONT_SIZE: f32 = 13.5;

pub struct TerminalPane {
    pub id: u32,
    pub title: String,
    pub screen: Screen,
    pub closed: bool,
    pub exit_code: Option<u32>,
    pub open: bool,
    last_sent_size: (u16, u16),
}

impl TerminalPane {
    pub fn new(id: u32, cols: u16, rows: u16) -> Self {
        Self {
            id,
            title: format!("Terminal #{id}"),
            screen: Screen::new(cols, rows),
            closed: false,
            exit_code: None,
            open: true,
            last_sent_size: (cols, rows),
        }
    }
}

/// Draw the terminal grid inside `ui`, returning any outgoing protocol messages
/// (keyboard input, resize, scroll) and whether this terminal was clicked to focus.
pub fn show_terminal(
    ui: &mut egui::Ui,
    term: &mut TerminalPane,
    focused: bool,
    out: &mut Vec<ClientMsg>,
) -> bool {
    let mut clicked = false;
    let font = FontId::monospace(FONT_SIZE);

    // Measure monospace cell geometry from the active font atlas.
    let (cell_w, cell_h) = ui.fonts_mut(|f| {
        let w = f.glyph_width(&font, 'W').max(6.0);
        let h = f.row_height(&font).max(12.0);
        (w, h)
    });

    let avail = ui.available_size().max(Vec2::new(cell_w * 20.0, cell_h * 6.0));
    let (rect, resp) = ui.allocate_exact_size(avail, Sense::click());

    if resp.clicked() {
        clicked = true;
    }

    // Dynamic grid resize when the window is resized.
    let pad = 6.0;
    let inner = rect.shrink(pad);
    let cols = ((inner.width() / cell_w).floor() as u16).clamp(16, 400);
    let rows = ((inner.height() / cell_h).floor() as u16).clamp(4, 160);
    if !term.closed && (cols, rows) != term.last_sent_size {
        term.last_sent_size = (cols, rows);
        out.push(ClientMsg::TermResize { term_id: term.id, cols, rows });
    }

    // Mouse wheel scrolling when pointer hovers over this terminal.
    if resp.hovered() && !term.closed {
        let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll_y.abs() >= 4.0 {
            let lines = (scroll_y / 16.0).round() as i32;
            if lines != 0 {
                out.push(ClientMsg::TermScroll { term_id: term.id, delta: lines });
            }
        }
    }

    // Keyboard & paste input when focused.
    if focused && !term.closed {
        let mut bytes = Vec::new();
        let app_cursor = term.screen.app_cursor;
        let bracketed = term.screen.bracketed_paste;
        ui.input(|i| {
            for ev in &i.events {
                match ev {
                    Event::Text(s) => {
                        // Ctrl combinations are translated via Event::Key below.
                        if !i.modifiers.ctrl && !i.modifiers.command {
                            bytes.extend_from_slice(s.as_bytes());
                        }
                    }
                    Event::Paste(s) => {
                        if bracketed {
                            bytes.extend_from_slice(b"\x1b[200~");
                            bytes.extend_from_slice(s.as_bytes());
                            bytes.extend_from_slice(b"\x1b[201~");
                        } else {
                            bytes.extend_from_slice(s.as_bytes());
                        }
                    }
                    Event::Key { key, pressed: true, modifiers, .. } => {
                        encode_key(*key, *modifiers, app_cursor, &mut bytes);
                    }
                    _ => {}
                }
            }
        });
        if !bytes.is_empty() {
            out.push(ClientMsg::TermInput { term_id: term.id, data: bytes });
        }
    }

    // Paint background & border.
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, theme::TERM_DEFAULT_BG);
    let border = if focused { theme::BORDER_FOCUS } else { theme::BORDER };
    painter.rect_stroke(rect, 3.0, Stroke::new(1.0, border), StrokeKind::Inside);

    // Paint every visible row from the retained Screen mirror.
    let origin = inner.min;
    for (r_idx, row) in term.screen.lines.iter().enumerate() {
        let y = origin.y + r_idx as f32 * cell_h;
        if y > inner.max.y {
            break;
        }
        let mut col: u16 = 0;
        for span in row {
            let x = origin.x + col as f32 * cell_w;
            let w = span.width as f32 * cell_w;
            let bold = span.style.flags.contains(StyleFlags::BOLD);
            let mut fg = theme::resolve_color(span.style.fg, theme::TERM_DEFAULT_FG, bold);
            let mut bg = theme::resolve_color(span.style.bg, theme::TERM_DEFAULT_BG, false);
            if span.style.flags.contains(StyleFlags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if span.style.flags.contains(StyleFlags::DIM) {
                fg = theme::dim_color(fg);
            }

            if bg != theme::TERM_DEFAULT_BG {
                let bg_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, cell_h));
                painter.rect_filled(bg_rect, 0.0, bg);
            }
            if !span.text.trim_end().is_empty() || span.style.flags.contains(StyleFlags::UNDERLINE) {
                painter.text(Pos2::new(x, y), Align2::LEFT_TOP, &span.text, font.clone(), fg);
            }
            if span.style.flags.contains(StyleFlags::UNDERLINE) {
                let uy = y + cell_h - 1.5;
                painter.line_segment([Pos2::new(x, uy), Pos2::new(x + w, uy)], Stroke::new(1.0, fg));
            }
            col = col.saturating_add(span.width);
        }
    }

    // Paint cursor when on the live viewport (not scrolled up).
    if term.screen.cursor_visible && term.screen.scrollback == 0 && !term.closed {
        let cx = origin.x + term.screen.cursor_col as f32 * cell_w;
        let cy = origin.y + term.screen.cursor_row as f32 * cell_h;
        let cur_rect = Rect::from_min_size(Pos2::new(cx, cy), Vec2::new(cell_w, cell_h));
        if focused {
            painter.rect_filled(cur_rect, 1.0, Color32::from_rgba_unmultiplied(56, 189, 248, 175));
        } else {
            painter.rect_stroke(cur_rect, 1.0, Stroke::new(1.0, theme::ACCENT_CYAN), StrokeKind::Inside);
        }
    }

    // Overlay badge when scrolled back or exited.
    if term.screen.scrollback > 0 {
        let badge = format!(" SCROLL +{} ", term.screen.scrollback);
        let pos = Pos2::new(inner.max.x - 4.0, inner.min.y + 2.0);
        painter.text(pos, Align2::RIGHT_TOP, badge, FontId::monospace(11.0), theme::ACCENT_AMBER);
    } else if term.closed {
        let msg = match term.exit_code {
            Some(c) => format!(" process exited ({c}) "),
            None => " process exited ".into(),
        };
        let pos = Pos2::new(inner.max.x - 4.0, inner.min.y + 2.0);
        painter.text(pos, Align2::RIGHT_TOP, msg, FontId::monospace(11.0), theme::ACCENT_CORAL);
    }

    clicked
}

fn encode_key(key: Key, m: Modifiers, app_cursor: bool, out: &mut Vec<u8>) {
    // Ctrl+A..Z control codes (unless Shift is held for e.g. Ctrl+Shift+C).
    if m.ctrl && !m.alt && !m.shift {
        if let Some(ctrl_byte) = ctrl_letter(key) {
            out.push(ctrl_byte);
            return;
        }
        match key {
            Key::OpenBracket => {
                out.push(0x1b);
                return;
            }
            Key::Backslash => {
                out.push(0x1c);
                return;
            }
            Key::CloseBracket => {
                out.push(0x1d);
                return;
            }
            Key::Space => {
                out.push(0x00);
                return;
            }
            _ => {}
        }
    }

    let csi_prefix: &[u8] = if app_cursor { b"\x1bO" } else { b"\x1b[" };
    match key {
        Key::Enter => out.push(b'\r'),
        Key::Backspace => out.push(0x7f),
        Key::Tab => out.extend_from_slice(if m.shift { b"\x1b[Z" } else { b"\t" }),
        Key::Escape => out.push(0x1b),
        Key::ArrowUp => {
            out.extend_from_slice(csi_prefix);
            out.push(b'A');
        }
        Key::ArrowDown => {
            out.extend_from_slice(csi_prefix);
            out.push(b'B');
        }
        Key::ArrowRight => {
            out.extend_from_slice(csi_prefix);
            out.push(b'C');
        }
        Key::ArrowLeft => {
            out.extend_from_slice(csi_prefix);
            out.push(b'D');
        }
        Key::Home => out.extend_from_slice(b"\x1b[H"),
        Key::End => out.extend_from_slice(b"\x1b[F"),
        Key::PageUp => out.extend_from_slice(b"\x1b[5~"),
        Key::PageDown => out.extend_from_slice(b"\x1b[6~"),
        Key::Insert => out.extend_from_slice(b"\x1b[2~"),
        Key::Delete => out.extend_from_slice(b"\x1b[3~"),
        Key::F1 => out.extend_from_slice(b"\x1bOP"),
        Key::F2 => out.extend_from_slice(b"\x1bOQ"),
        Key::F3 => out.extend_from_slice(b"\x1bOR"),
        Key::F4 => out.extend_from_slice(b"\x1bOS"),
        Key::F5 => out.extend_from_slice(b"\x1b[15~"),
        Key::F6 => out.extend_from_slice(b"\x1b[17~"),
        Key::F7 => out.extend_from_slice(b"\x1b[18~"),
        Key::F8 => out.extend_from_slice(b"\x1b[19~"),
        Key::F9 => out.extend_from_slice(b"\x1b[20~"),
        Key::F10 => out.extend_from_slice(b"\x1b[21~"),
        Key::F11 => out.extend_from_slice(b"\x1b[23~"),
        Key::F12 => out.extend_from_slice(b"\x1b[24~"),
        _ => {}
    }
}

fn ctrl_letter(key: Key) -> Option<u8> {
    let ch = match key {
        Key::A => b'A',
        Key::B => b'B',
        Key::C => b'C',
        Key::D => b'D',
        Key::E => b'E',
        Key::F => b'F',
        Key::G => b'G',
        Key::H => b'H',
        Key::I => b'I',
        Key::J => b'J',
        Key::K => b'K',
        Key::L => b'L',
        Key::M => b'M',
        Key::N => b'N',
        Key::O => b'O',
        Key::P => b'P',
        Key::Q => b'Q',
        Key::R => b'R',
        Key::S => b'S',
        Key::T => b'T',
        Key::U => b'U',
        Key::V => b'V',
        Key::W => b'W',
        Key::X => b'X',
        Key::Y => b'Y',
        Key::Z => b'Z',
        _ => return None,
    };
    Some(ch - b'A' + 1)
}
