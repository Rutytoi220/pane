//! Native GPU compositor view for streamed remote X11 GUI windows (`[APP]`).
//!
//! Each remote window owns a single `egui::TextureHandle`; incoming
//! `HostMsg::GuiWindowDamage` packets are LZ4-decompressed and patched directly
//! into the GPU texture via `TextureHandle::set_partial` without reallocating.

use egui::{
    Color32, ColorImage, Event, EventFilter, Key, PointerButton, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle,
    TextureOptions, Vec2,
};
use pane_proto::ClientMsg;

use crate::theme;

const XK_CONTROL_L: u32 = 0xffe3;
const XK_SHIFT_L: u32 = 0xffe1;

pub struct GuiWindowPane {
    pub id: u32,
    pub workspace: u8,
    pub title: String,
    pub width: u16,
    pub height: u16,
    pub open: bool,
    pub active_sent: bool,
    pub want_focus: bool,
    texture: Option<TextureHandle>,
    last_mouse: Option<(i16, i16)>,
}

impl GuiWindowPane {
    pub fn new(id: u32, workspace: u8, title: String, width: u16, height: u16) -> Self {
        Self {
            id,
            workspace,
            title,
            width: width.max(1),
            height: height.max(1),
            open: true,
            active_sent: true,
            want_focus: true,
            texture: None,
            last_mouse: None,
        }
    }

    fn ensure_texture(&mut self, ctx: &egui::Context) -> &mut TextureHandle {
        if self.texture.is_none() {
            let img = ColorImage::filled([self.width as usize, self.height as usize], theme::BG_TERM);
            let tex = ctx.load_texture(format!("pane_gui_win_{}", self.id), img, TextureOptions::LINEAR);
            self.texture = Some(tex);
        }
        self.texture.as_mut().unwrap()
    }

    pub fn apply_resize(&mut self, ctx: &egui::Context, width: u16, height: u16) {
        let w = width.max(1);
        let h = height.max(1);
        if (self.width, self.height) != (w, h) {
            self.width = w;
            self.height = h;
            let img = ColorImage::filled([w as usize, h as usize], theme::BG_TERM);
            let tex = ctx.load_texture(format!("pane_gui_win_{}", self.id), img, TextureOptions::LINEAR);
            self.texture = Some(tex);
        }
    }

    /// Decompress an LZ4 RGBA sub-rectangle and patch it into the existing GPU
    /// texture in-place via `TextureHandle::set_partial`.
    pub fn apply_damage(
        &mut self,
        ctx: &egui::Context,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        rgba_lz4: &[u8],
    ) {
        if width == 0 || height == 0 {
            return;
        }
        let max_w = self.width as usize;
        let max_h = self.height as usize;
        if (x as usize) + (width as usize) > max_w || (y as usize) + (height as usize) > max_h {
            return;
        }
        let Ok(rgba) = lz4_flex::decompress_size_prepended(rgba_lz4) else {
            return;
        };
        let expected = (width as usize) * (height as usize) * 4;
        if rgba.len() != expected {
            return;
        }
        let patch = ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba);
        let tex = self.ensure_texture(ctx);
        tex.set_partial([x as usize, y as usize], patch, TextureOptions::LINEAR);
    }
}

/// Render the streamed GUI window texture and forward pointer, scroll, and
/// keyboard events back to `pane-host`. Returns `true` if clicked to focus.
pub fn show_gui_window(
    ui: &mut egui::Ui,
    win: &mut GuiWindowPane,
    focused: bool,
    out: &mut Vec<ClientMsg>,
) -> bool {
    let mut clicked = false;
    let avail = ui.available_size().max(Vec2::new(160.0, 120.0));
    let (outer_rect, resp) = ui.allocate_exact_size(avail, Sense::click_and_drag());

    let pointer_pressed_inside =
        resp.hovered() && ui.input(|i| i.pointer.any_pressed());
    if resp.clicked() || resp.drag_started() || resp.is_pointer_button_down_on() || pointer_pressed_inside {
        clicked = true;
        win.want_focus = true;
    }

    if win.want_focus || (focused && !resp.has_focus() && !ui.ctx().egui_wants_keyboard_input()) {
        resp.request_focus();
        win.want_focus = false;
    }
    let active_kbd = focused && (resp.has_focus() || !ui.ctx().egui_wants_keyboard_input());
    if active_kbd && resp.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                resp.id,
                EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
            );
        });
    }

    // Fit the remote framebuffer cleanly inside `outer_rect` preserving aspect ratio.
    let src_w = f32::from(win.width).max(1.0);
    let src_h = f32::from(win.height).max(1.0);
    let scale = (outer_rect.width() / src_w).min(outer_rect.height() / src_h).max(0.05);
    let draw_size = Vec2::new(src_w * scale, src_h * scale);
    let img_rect = Rect::from_center_size(outer_rect.center(), draw_size);

    let painter = ui.painter_at(outer_rect);
    painter.rect_filled(outer_rect, 2.0, theme::BG_TERM);

    if let Some(tex) = &win.texture {
        let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
        painter.image(tex.id(), img_rect, uv, Color32::WHITE);
    } else {
        painter.text(
            outer_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Waiting for first frame from Xvfb...",
            egui::FontId::monospace(12.0),
            theme::TEXT_MUTED,
        );
    }

    let border = if active_kbd { theme::BORDER_FOCUS } else { theme::BORDER };
    painter.rect_stroke(outer_rect, 2.0, Stroke::new(1.0, border), StrokeKind::Inside);

    // Map local pointer position inside `img_rect` to remote X11 window coordinates.
    let map_pos = |pos: Pos2| -> (i16, i16) {
        let nx = ((pos.x - img_rect.left()) / img_rect.width().max(1.0)).clamp(0.0, 1.0);
        let ny = ((pos.y - img_rect.top()) / img_rect.height().max(1.0)).clamp(0.0, 1.0);
        let rx = (nx * (src_w - 1.0)).round() as i16;
        let ry = (ny * (src_h - 1.0)).round() as i16;
        (rx, ry)
    };

    // Pointer move, mouse button press/release, and scroll wheel forwarding.
    let pointer_over_canvas = resp.hovered() || resp.dragged() || resp.is_pointer_button_down_on();
    if pointer_over_canvas {
        let hover_pos = ui.input(|i| i.pointer.hover_pos().or_else(|| i.pointer.interact_pos()));
        if let Some(pos) = hover_pos {
            let (rx, ry) = map_pos(pos);
            if win.last_mouse != Some((rx, ry)) {
                win.last_mouse = Some((rx, ry));
                out.push(ClientMsg::GuiMouseMove { win_id: win.id, x: rx, y: ry });
            }

            ui.input(|i| {
                for ev in &i.events {
                    if let Event::PointerButton { pos, button, pressed, .. } = ev
                        && outer_rect.contains(*pos)
                    {
                        let (bx, by) = map_pos(*pos);
                        let x11_btn = match button {
                            PointerButton::Primary => 1,
                            PointerButton::Middle => 2,
                            PointerButton::Secondary => 3,
                            PointerButton::Extra1 => 8,
                            PointerButton::Extra2 => 9,
                        };
                        out.push(ClientMsg::GuiMouseButton {
                            win_id: win.id,
                            button: x11_btn,
                            pressed: *pressed,
                            x: bx,
                            y: by,
                        });
                    }
                }
            });

            let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll_y.abs() >= 4.0 {
                out.push(ClientMsg::GuiScroll {
                    win_id: win.id,
                    delta_y: scroll_y.round().clamp(-500.0, 500.0) as i16,
                    x: rx,
                    y: ry,
                });
            }
        }
    }

    // Keyboard forwarding when this `[APP]` window is focused.
    if active_kbd {
        let win_id = win.id;
        ui.input_mut(|i| {
            let cur_mods = i.modifiers;
            i.events.retain(|ev| match ev {
                Event::Copy => {
                    send_combo(win_id, XK_CONTROL_L, b'c' as u32, out);
                    false
                }
                Event::Cut => {
                    send_combo(win_id, XK_CONTROL_L, b'x' as u32, out);
                    false
                }
                Event::Text(s) => {
                    if !cur_mods.ctrl && !cur_mods.command && !cur_mods.alt {
                        for ch in s.chars() {
                            if let Some(ks) = char_to_keysym(ch) {
                                out.push(ClientMsg::GuiKey { win_id, keysym: ks, pressed: true });
                                out.push(ClientMsg::GuiKey { win_id, keysym: ks, pressed: false });
                            }
                        }
                        false
                    } else {
                        true
                    }
                }
                Event::Key { key, pressed, modifiers, .. } => {
                    // Printable single characters without Ctrl/Alt are handled by Event::Text
                    // so keyboard layouts & punctuation map accurately; special keys and Ctrl/Alt
                    // shortcuts go through Event::Key.
                    let is_ctrl_or_alt = modifiers.ctrl || modifiers.command || modifiers.alt;
                    if let Some(ks) = special_key_to_keysym(*key) {
                        if is_ctrl_or_alt && *pressed {
                            if modifiers.ctrl || modifiers.command {
                                out.push(ClientMsg::GuiKey { win_id, keysym: XK_CONTROL_L, pressed: true });
                            }
                            out.push(ClientMsg::GuiKey { win_id, keysym: ks, pressed: true });
                            out.push(ClientMsg::GuiKey { win_id, keysym: ks, pressed: false });
                            if modifiers.ctrl || modifiers.command {
                                out.push(ClientMsg::GuiKey { win_id, keysym: XK_CONTROL_L, pressed: false });
                            }
                        } else {
                            out.push(ClientMsg::GuiKey { win_id, keysym: ks, pressed: *pressed });
                        }
                        false
                    } else if is_ctrl_or_alt && *pressed {
                        if let Some(ks) = letter_key_to_keysym(*key, modifiers.shift) {
                            if modifiers.shift {
                                out.push(ClientMsg::GuiKey { win_id, keysym: XK_SHIFT_L, pressed: true });
                            }
                            if modifiers.ctrl || modifiers.command {
                                out.push(ClientMsg::GuiKey { win_id, keysym: XK_CONTROL_L, pressed: true });
                            }
                            out.push(ClientMsg::GuiKey { win_id, keysym: ks, pressed: true });
                            out.push(ClientMsg::GuiKey { win_id, keysym: ks, pressed: false });
                            if modifiers.ctrl || modifiers.command {
                                out.push(ClientMsg::GuiKey { win_id, keysym: XK_CONTROL_L, pressed: false });
                            }
                            if modifiers.shift {
                                out.push(ClientMsg::GuiKey { win_id, keysym: XK_SHIFT_L, pressed: false });
                            }
                            false
                        } else {
                            true
                        }
                    } else {
                        true
                    }
                }
                _ => true,
            });
        });
    }

    clicked
}

fn send_combo(win_id: u32, modifier_ks: u32, key_ks: u32, out: &mut Vec<ClientMsg>) {
    out.push(ClientMsg::GuiKey { win_id, keysym: modifier_ks, pressed: true });
    out.push(ClientMsg::GuiKey { win_id, keysym: key_ks, pressed: true });
    out.push(ClientMsg::GuiKey { win_id, keysym: key_ks, pressed: false });
    out.push(ClientMsg::GuiKey { win_id, keysym: modifier_ks, pressed: false });
}

fn char_to_keysym(ch: char) -> Option<u32> {
    let cp = ch as u32;
    if (0x20..=0x7e).contains(&cp) {
        Some(cp)
    } else if cp >= 0x100 {
        Some(0x0100_0000 | cp)
    } else {
        None
    }
}

fn special_key_to_keysym(key: Key) -> Option<u32> {
    Some(match key {
        Key::Enter => 0xff0d,
        Key::Backspace => 0xff08,
        Key::Tab => 0xff09,
        Key::Escape => 0xff1b,
        Key::Insert => 0xff63,
        Key::Delete => 0xffff,
        Key::Home => 0xff50,
        Key::End => 0xff57,
        Key::PageUp => 0xff55,
        Key::PageDown => 0xff56,
        Key::ArrowLeft => 0xff51,
        Key::ArrowUp => 0xff52,
        Key::ArrowRight => 0xff53,
        Key::ArrowDown => 0xff54,
        Key::F1 => 0xffbe,
        Key::F2 => 0xffbf,
        Key::F3 => 0xffc0,
        Key::F4 => 0xffc1,
        Key::F5 => 0xffc2,
        Key::F6 => 0xffc3,
        Key::F7 => 0xffc4,
        Key::F8 => 0xffc5,
        Key::F9 => 0xffc6,
        Key::F10 => 0xffc7,
        Key::F11 => 0xffc8,
        Key::F12 => 0xffc9,
        _ => return None,
    })
}

fn letter_key_to_keysym(key: Key, shift: bool) -> Option<u32> {
    let base = match key {
        Key::A => b'a',
        Key::B => b'b',
        Key::C => b'c',
        Key::D => b'd',
        Key::E => b'e',
        Key::F => b'f',
        Key::G => b'g',
        Key::H => b'h',
        Key::I => b'i',
        Key::J => b'j',
        Key::K => b'k',
        Key::L => b'l',
        Key::M => b'm',
        Key::N => b'n',
        Key::O => b'o',
        Key::P => b'p',
        Key::Q => b'q',
        Key::R => b'r',
        Key::S => b's',
        Key::T => b't',
        Key::U => b'u',
        Key::V => b'v',
        Key::W => b'w',
        Key::X => b'x',
        Key::Y => b'y',
        Key::Z => b'z',
        _ => return None,
    };
    Some(if shift { base.to_ascii_uppercase() } else { base } as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_ascii_and_special_keys_to_x11_keysyms() {
        assert_eq!(char_to_keysym('a'), Some(0x61));
        assert_eq!(char_to_keysym(' '), Some(0x20));
        assert_eq!(special_key_to_keysym(Key::Enter), Some(0xff0d));
        assert_eq!(special_key_to_keysym(Key::Escape), Some(0xff1b));
        assert_eq!(special_key_to_keysym(Key::ArrowLeft), Some(0xff51));
        assert_eq!(letter_key_to_keysym(Key::C, false), Some(b'c' as u32));
        assert_eq!(letter_key_to_keysym(Key::C, true), Some(b'C' as u32));
    }
}
