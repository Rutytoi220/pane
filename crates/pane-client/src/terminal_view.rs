//! GPU-rendered terminal viewport: draws retained `pane_proto::Screen` state
//! directly via egui's tessellator and converts keyboard/shortcut/paste/scroll/
//! resize events back into `ClientMsg` commands.

use egui::{Align2, Color32, Event, EventFilter, FontId, Key, Modifiers, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use pane_proto::{ClientMsg, Screen, StyleFlags};

use crate::theme;

const FONT_SIZE: f32 = 13.5;

pub struct TerminalPane {
    pub id: u32,
    pub workspace: u8,
    pub title: String,
    pub screen: Screen,
    pub closed: bool,
    pub exit_code: Option<u32>,
    pub open: bool,
    pub want_focus: bool,
    last_sent_size: (u16, u16),
}

impl TerminalPane {
    pub fn new(id: u32, workspace: u8, cols: u16, rows: u16) -> Self {
        Self {
            id,
            workspace,
            title: format!("Terminal #{id}"),
            screen: Screen::new(cols, rows),
            closed: false,
            exit_code: None,
            open: true,
            want_focus: true,
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
        term.want_focus = true;
    }

    // Properly acquire egui widget focus when active so TextEdits release focus
    // and Tab/Esc/Arrow keys stay locked inside the terminal instead of moving UI focus.
    if term.want_focus || (focused && !resp.has_focus() && !ui.ctx().egui_wants_keyboard_input()) {
        resp.request_focus();
        term.want_focus = false;
    }
    let active_kbd = focused && !term.closed && (resp.has_focus() || !ui.ctx().egui_wants_keyboard_input());
    if active_kbd && resp.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                resp.id,
                EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
            );
        });
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

    // Keyboard, shortcut & paste input when this terminal has keyboard focus.
    // We consume handled events so egui never double-triggers buttons or quit/zoom shortcuts.
    if active_kbd {
        let mut bytes = Vec::new();
        let mut copy_screen = false;
        let app_cursor = term.screen.app_cursor;
        let bracketed = term.screen.bracketed_paste;

        ui.input_mut(|i| {
            let cur_mods = i.modifiers;
            i.events.retain(|ev| {
                let outcome = translate_event(ev, cur_mods, app_cursor, bracketed, &mut bytes);
                if outcome.copy_screen {
                    copy_screen = true;
                }
                !outcome.consumed
            });
        });

        if copy_screen {
            ui.ctx().copy_text(term.screen.text());
        }
        if !bytes.is_empty() {
            out.push(ClientMsg::TermInput { term_id: term.id, data: bytes });
        }
    }

    // Paint background & border.
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, theme::TERM_DEFAULT_BG);
    let border = if active_kbd { theme::BORDER_FOCUS } else { theme::BORDER };
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
        if active_kbd {
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

struct EventOutcome {
    consumed: bool,
    copy_screen: bool,
}

fn translate_event(
    ev: &Event,
    cur_mods: Modifiers,
    app_cursor: bool,
    bracketed: bool,
    out: &mut Vec<u8>,
) -> EventOutcome {
    match ev {
        // egui-winit converts Ctrl+C -> Event::Copy and Ctrl+X -> Event::Cut before
        // emitting Event::Key. Inside a terminal, those must deliver SIGINT (^C) and ^X!
        Event::Copy => {
            out.push(0x03);
            EventOutcome { consumed: true, copy_screen: false }
        }
        Event::Cut => {
            out.push(0x18);
            EventOutcome { consumed: true, copy_screen: false }
        }
        Event::Paste(s) => {
            if bracketed {
                out.extend_from_slice(b"\x1b[200~");
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\x1b[201~");
            } else {
                out.extend_from_slice(s.as_bytes());
            }
            EventOutcome { consumed: true, copy_screen: false }
        }
        Event::Text(s) => {
            if !cur_mods.ctrl && !cur_mods.command && !cur_mods.alt {
                out.extend_from_slice(s.as_bytes());
                EventOutcome { consumed: true, copy_screen: false }
            } else {
                EventOutcome { consumed: false, copy_screen: false }
            }
        }
        Event::Key { key, pressed: true, modifiers, .. } => {
            // Ctrl+Shift+C copies the visible terminal buffer to clipboard.
            if (modifiers.ctrl || modifiers.command) && modifiers.shift && !modifiers.alt && *key == Key::C {
                return EventOutcome { consumed: true, copy_screen: true };
            }
            let before = out.len();
            encode_key(*key, *modifiers, app_cursor, out);
            EventOutcome { consumed: out.len() > before, copy_screen: false }
        }
        _ => EventOutcome { consumed: false, copy_screen: false },
    }
}

fn encode_key(key: Key, m: Modifiers, app_cursor: bool, out: &mut Vec<u8>) {
    let ctrl = m.ctrl || m.command;

    // Ctrl + key control codes (Ctrl+A..Z, Ctrl+[, Ctrl+\, Ctrl+], Ctrl+^, Ctrl+_, Ctrl+2).
    if ctrl && !m.shift {
        let ctrl_byte = ctrl_letter(key).or(match key {
            Key::OpenBracket => Some(0x1b),
            Key::Backslash => Some(0x1c),
            Key::CloseBracket => Some(0x1d),
            Key::Num6 => Some(0x1e),
            Key::Minus | Key::Slash => Some(0x1f),
            Key::Space | Key::Num2 => Some(0x00),
            Key::Backspace => Some(0x08),
            _ => None,
        });
        if let Some(b) = ctrl_byte {
            if m.alt {
                out.push(0x1b);
            }
            out.push(b);
            return;
        }
    }

    // Alt + ASCII letter / Enter / Backspace -> Meta ESC prefix (bash/zsh/emacs/nano).
    if m.alt && !ctrl {
        if let Some(ch) = key_to_ascii_letter(key) {
            out.push(0x1b);
            out.push(if m.shift { ch } else { ch.to_ascii_lowercase() });
            return;
        }
        match key {
            Key::Backspace => {
                out.extend_from_slice(b"\x1b\x7f");
                return;
            }
            Key::Enter => {
                out.extend_from_slice(b"\x1b\r");
                return;
            }
            _ => {}
        }
    }

    // Xterm modifier parameter for arrows / Home / End / Insert / Delete / PageUp / PageDown.
    let mod_param = 1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(ctrl);
    let csi_prefix: &[u8] = if app_cursor && mod_param == 1 { b"\x1bO" } else { b"\x1b[" };

    let push_modified_csi = |final_byte: u8, out: &mut Vec<u8>| {
        if mod_param == 1 {
            out.extend_from_slice(csi_prefix);
            out.push(final_byte);
        } else {
            out.extend_from_slice(format!("\x1b[1;{mod_param}{}", final_byte as char).as_bytes());
        }
    };

    match key {
        Key::Enter => out.push(b'\r'),
        Key::Backspace => out.push(0x7f),
        Key::Tab => out.extend_from_slice(if m.shift { b"\x1b[Z" } else { b"\t" }),
        Key::Escape => out.push(0x1b),
        Key::ArrowUp => push_modified_csi(b'A', out),
        Key::ArrowDown => push_modified_csi(b'B', out),
        Key::ArrowRight => push_modified_csi(b'C', out),
        Key::ArrowLeft => push_modified_csi(b'D', out),
        Key::Home => push_modified_csi(b'H', out),
        Key::End => push_modified_csi(b'F', out),
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
    key_to_ascii_letter(key).map(|ch| ch - b'A' + 1)
}

fn key_to_ascii_letter(key: Key) -> Option<u8> {
    Some(match key {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_event(key: Key, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    #[test]
    fn translates_ctrl_c_ctrl_x_ctrl_q_and_ctrl_arrows() {
        let mut out = Vec::new();
        // Ctrl+C arrives from egui-winit as Event::Copy.
        assert!(translate_event(&Event::Copy, Modifiers::CTRL, false, false, &mut out).consumed);
        // Ctrl+X arrives from egui-winit as Event::Cut.
        assert!(translate_event(&Event::Cut, Modifiers::CTRL, false, false, &mut out).consumed);
        // Ctrl+Q arrives as Key::Q with CTRL|COMMAND.
        let ctrl_cmd = Modifiers { ctrl: true, command: true, ..Default::default() };
        assert!(translate_event(&key_event(Key::Q, ctrl_cmd), ctrl_cmd, false, false, &mut out).consumed);
        assert!(translate_event(&key_event(Key::D, ctrl_cmd), ctrl_cmd, false, false, &mut out).consumed);
        assert_eq!(out, vec![0x03, 0x18, 0x11, 0x04]);

        // Ctrl+Right sends xterm modifier CSI sequence.
        let mut nav = Vec::new();
        assert!(translate_event(&key_event(Key::ArrowRight, ctrl_cmd), ctrl_cmd, false, false, &mut nav).consumed);
        assert_eq!(nav, b"\x1b[1;5C");

        // Alt+B sends ESC b.
        let mut meta = Vec::new();
        assert!(translate_event(&key_event(Key::B, Modifiers::ALT), Modifiers::ALT, false, false, &mut meta).consumed);
        assert_eq!(meta, b"\x1bb");

        // Ctrl+Shift+C triggers screen copy rather than sending bytes.
        let mut cp = Vec::new();
        let ctrl_shift = Modifiers { ctrl: true, command: true, shift: true, ..Default::default() };
        let res = translate_event(&key_event(Key::C, ctrl_shift), ctrl_shift, false, false, &mut cp);
        assert!(res.consumed && res.copy_screen);
        assert!(cp.is_empty());
    }
}
