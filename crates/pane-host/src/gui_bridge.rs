//! X11/Xwayland live display auto-detection + headless Xvfb fallback, with
//! XDamage sub-rectangle capture and XTEST input forwarding.
//!
//! Pure-Rust `x11rb` (`RustConnection`, zero `libxcb` or C X11 dependencies)
//! keeps `pane-host` 100% static-musl compatible.
//!
//! Connection priority:
//! 1. Probe live host displays `/tmp/.X11-unix/X0` (`:0`) and `X1` (`:1`) using
//!    `~/.Xauthority` or `/run/user/<uid>/gdm/Xauthority` (and Xwayland auth cookies).
//! 2. Fall back to spawning headless `Xvfb :99` only when no live desktop exists.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use pane_proto::HostMsg;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::composite;
use x11rb::protocol::damage;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageData, ClientMessageEvent, ConfigureWindowAux,
    ConnectionExt as XprotoExt, EventMask, ImageFormat, InputFocus, StackMode, Window,
};
use x11rb::protocol::xtest;
use x11rb::rust_connection::RustConnection;

use crate::outbox::Outbox;

const FALLBACK_DISPLAY: &str = ":99";
/// Coalesced damage capture cadence (~30 FPS max per active window).
const CAPTURE_TICK: Duration = Duration::from_millis(33);

const XK_SHIFT_L: u32 = 0xffe1;
const XK_SHIFT_R: u32 = 0xffe2;

enum Cmd {
    Launch { req_id: u64, command: String },
    AttachDesktop { req_id: u64 },
    ResizeDesktopView { win_id: u32, width: u16, height: u16 },
    MouseMove { win_id: u32, x: i16, y: i16 },
    MouseButton { win_id: u32, button: u8, pressed: bool, x: i16, y: i16 },
    Scroll { win_id: u32, delta_y: i16, x: i16, y: i16 },
    Key { win_id: u32, keysym: u32, pressed: bool },
    CloseWindow { win_id: u32 },
    SetActive { win_id: u32, active: bool },
    Shutdown,
}

pub struct GuiBridge {
    tx: Sender<Cmd>,
    handle: Option<thread::JoinHandle<()>>,
}

impl GuiBridge {
    pub fn spawn(outbox: Outbox) -> Self {
        let (tx, rx) = unbounded();
        let handle = thread::Builder::new()
            .name("gui-bridge".into())
            .spawn(move || bridge_loop(rx, outbox))
            .expect("spawn gui bridge");
        Self { tx, handle: Some(handle) }
    }

    pub fn launch(&self, req_id: u64, command: String) {
        let _ = self.tx.send(Cmd::Launch { req_id, command });
    }

    pub fn attach_desktop(&self, req_id: u64) {
        let _ = self.tx.send(Cmd::AttachDesktop { req_id });
    }

    pub fn resize_desktop_view(&self, win_id: u32, width: u16, height: u16) {
        let _ = self.tx.send(Cmd::ResizeDesktopView { win_id, width, height });
    }

    pub fn mouse_move(&self, win_id: u32, x: i16, y: i16) {
        let _ = self.tx.send(Cmd::MouseMove { win_id, x, y });
    }

    pub fn mouse_button(&self, win_id: u32, button: u8, pressed: bool, x: i16, y: i16) {
        let _ = self.tx.send(Cmd::MouseButton { win_id, button, pressed, x, y });
    }

    pub fn scroll(&self, win_id: u32, delta_y: i16, x: i16, y: i16) {
        let _ = self.tx.send(Cmd::Scroll { win_id, delta_y, x, y });
    }

    pub fn key(&self, win_id: u32, keysym: u32, pressed: bool) {
        let _ = self.tx.send(Cmd::Key { win_id, keysym, pressed });
    }

    pub fn close_window(&self, win_id: u32) {
        let _ = self.tx.send(Cmd::CloseWindow { win_id });
    }

    pub fn set_active(&self, win_id: u32, active: bool) {
        let _ = self.tx.send(Cmd::SetActive { win_id, active });
    }

    pub fn shutdown(mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirtyRect {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}

impl DirtyRect {
    fn union(self, other: Self) -> Self {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self.x.saturating_add(self.w).max(other.x.saturating_add(other.w));
        let y1 = self.y.saturating_add(self.h).max(other.y.saturating_add(other.h));
        Self { x: x0, y: y0, w: x1.saturating_sub(x0), h: y1.saturating_sub(y0) }
    }

    fn clamp(self, max_w: u16, max_h: u16) -> Option<Self> {
        if self.x >= max_w || self.y >= max_h || self.w == 0 || self.h == 0 {
            return None;
        }
        let w = self.w.min(max_w.saturating_sub(self.x));
        let h = self.h.min(max_h.saturating_sub(self.y));
        if w == 0 || h == 0 { None } else { Some(Self { x: self.x, y: self.y, w, h }) }
    }
}

struct TrackedWindow {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    damage: damage::Damage,
    active: bool,
    is_root: bool,
    dirty: Option<DirtyRect>,
}

struct X11State {
    conn: RustConnection,
    display_name: String,
    xauthority: Option<PathBuf>,
    root: Window,
    xvfb: Option<Child>,
    atoms: Atoms,
    windows: HashMap<Window, TrackedWindow>,
    damage_to_win: HashMap<damage::Damage, Window>,
    keymap: HashMap<u32, (u8, bool)>,
    shift_keycode: Option<u8>,
    shift_held: bool,
    focused_win: Option<Window>,
}

struct Atoms {
    net_wm_name: Atom,
    utf8_string: Atom,
    wm_protocols: Atom,
    wm_delete_window: Atom,
}

impl Drop for X11State {
    fn drop(&mut self) {
        if let Some(mut child) = self.xvfb.take() {
            log!("stopping Xvfb {}", self.display_name);
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn bridge_loop(rx: Receiver<Cmd>, outbox: Outbox) {
    let mut x11: Option<X11State> = None;
    let tick = crossbeam_channel::tick(CAPTURE_TICK);

    loop {
        crossbeam_channel::select! {
            recv(rx) -> cmd => match cmd {
                Ok(Cmd::Launch { req_id, command }) => {
                    if x11.is_none() {
                        match init_x11() {
                            Ok(st) => x11 = Some(st),
                            Err(msg) => {
                                log!("gui bridge init failed: {msg}");
                                outbox.send(HostMsg::Error { req_id: Some(req_id), message: msg });
                                continue;
                            }
                        }
                    }
                    if let Some(st) = x11.as_ref()
                        && let Err(msg) = spawn_gui_command(&command, &st.display_name, st.xauthority.as_deref())
                    {
                        outbox.send(HostMsg::Error { req_id: Some(req_id), message: msg });
                    }
                }
                Ok(Cmd::AttachDesktop { req_id }) => {
                    if x11.is_none() {
                        match init_x11() {
                            Ok(st) => x11 = Some(st),
                            Err(msg) => {
                                log!("attach desktop init failed: {msg}");
                                outbox.send(HostMsg::Error { req_id: Some(req_id), message: msg });
                                continue;
                            }
                        }
                    }
                    if let Some(st) = x11.as_mut()
                        && let Err(msg) = st.attach_root_desktop(&outbox)
                    {
                        outbox.send(HostMsg::Error { req_id: Some(req_id), message: msg });
                    }
                }
                Ok(Cmd::ResizeDesktopView { win_id, width, height }) => {
                    if let Some(st) = x11.as_mut()
                        && let Some(tw) = st.windows.get_mut(&win_id)
                    {
                        log!("desktop view hint for 0x{win_id:x}: {width}x{height} (native {}x{})", tw.width, tw.height);
                    }
                }
                Ok(Cmd::MouseMove { win_id, x, y }) => {
                    if let Some(st) = x11.as_mut() {
                        st.handle_mouse_move(win_id, x, y);
                    }
                }
                Ok(Cmd::MouseButton { win_id, button, pressed, x, y }) => {
                    if let Some(st) = x11.as_mut() {
                        st.handle_mouse_button(win_id, button, pressed, x, y);
                    }
                }
                Ok(Cmd::Scroll { win_id, delta_y, x, y }) => {
                    if let Some(st) = x11.as_mut() {
                        st.handle_scroll(win_id, delta_y, x, y);
                    }
                }
                Ok(Cmd::Key { win_id, keysym, pressed }) => {
                    if let Some(st) = x11.as_mut() {
                        st.handle_key(win_id, keysym, pressed);
                    }
                }
                Ok(Cmd::CloseWindow { win_id }) => {
                    if let Some(st) = x11.as_mut() {
                        st.handle_close_window(win_id, &outbox);
                    }
                }
                Ok(Cmd::SetActive { win_id, active }) => {
                    if let Some(st) = x11.as_mut()
                        && let Some(w) = st.windows.get_mut(&win_id)
                    {
                        let was_active = w.active;
                        w.active = active;
                        if active && !was_active {
                            w.dirty = Some(DirtyRect { x: 0, y: 0, w: w.width, h: w.height });
                        }
                    }
                }
                Ok(Cmd::Shutdown) | Err(_) => break,
            },
            recv(tick) -> _ => {
                if let Some(st) = x11.as_mut() {
                    st.poll_events(&outbox);
                    st.flush_damage(&outbox);
                }
            }
        }
    }
    drop(x11);
}

/// Candidate Xauthority cookie files on standard Linux desktops (LightDM/XFCE,
/// GDM/GNOME, KDE/SDDM, and Xwayland sessions).
fn discover_xauthority_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(env_val) = std::env::var_os("XAUTHORITY") {
        let p = PathBuf::from(env_val);
        if p.is_file() {
            out.push(p);
        }
    }
    let home_xauth = crate::files::home_dir().join(".Xauthority");
    if home_xauth.is_file() && !out.contains(&home_xauth) {
        out.push(home_xauth);
    }
    let uid = unsafe { libc::geteuid() };
    let run_user = PathBuf::from(format!("/run/user/{uid}"));
    let gdm = run_user.join("gdm/Xauthority");
    if gdm.is_file() && !out.contains(&gdm) {
        out.push(gdm);
    }
    if let Ok(entries) = std::fs::read_dir(&run_user) {
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if (s.starts_with(".mutter-Xwaylandauth.") || s.starts_with("xauth_")) && entry.path().is_file() {
                let p = entry.path();
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// Probe live X11/Xwayland displays (`:0`, `:1`) before falling back to `Xvfb :99`.
fn try_connect_live_display() -> Option<(RustConnection, usize, String, Option<PathBuf>)> {
    let xauths = discover_xauthority_candidates();

    let mut displays: Vec<String> = Vec::new();
    for (sock, dpy) in [("/tmp/.X11-unix/X0", ":0"), ("/tmp/.X11-unix/X1", ":1")] {
        if Path::new(sock).exists() {
            displays.push(dpy.to_owned());
        }
    }

    for dpy in displays {
        // First try with each discovered Xauthority cookie (most common on Ubuntu/Xubuntu/Debian).
        for xauth in &xauths {
            // SAFETY: called only on the single gui-bridge thread during lazy initialization.
            unsafe { std::env::set_var("XAUTHORITY", xauth) };
            if let Ok((conn, screen)) = RustConnection::connect(Some(&dpy)) {
                log!("attached to live X11 display {dpy} (XAUTHORITY={})", xauth.display());
                return Some((conn, screen, dpy, Some(xauth.clone())));
            }
        }
        // Also try without XAUTHORITY (e.g. `xhost +local:` or unauthenticated local display).
        unsafe { std::env::remove_var("XAUTHORITY") };
        if let Ok((conn, screen)) = RustConnection::connect(Some(&dpy)) {
            log!("attached to live X11 display {dpy} (no XAUTHORITY needed)");
            return Some((conn, screen, dpy, None));
        }
    }
    None
}

fn init_x11() -> Result<X11State, String> {
    // 1. Prefer an existing live desktop session on :0 or :1.
    let (conn, screen_num, display_name, xauthority, xvfb) = if let Some((c, s, dpy, xauth)) = try_connect_live_display()
    {
        (c, s, dpy, xauth, None)
    } else if let Ok((c, s)) = RustConnection::connect(Some(FALLBACK_DISPLAY)) {
        // 2. Reuse an existing :99 Xvfb if already up.
        log!("connected to existing X11 display {FALLBACK_DISPLAY}");
        (c, s, FALLBACK_DISPLAY.to_owned(), None, None)
    } else {
        // 3. Truly headless host: lazily spawn Xvfb :99.
        log!("no live :0/:1 display found; spawning Xvfb {FALLBACK_DISPLAY} -screen 0 1920x1080x24 -ac -nolisten tcp");
        let mut child = Command::new("Xvfb")
            .args([FALLBACK_DISPLAY, "-screen", "0", "1920x1080x24", "-ac", "-nolisten", "tcp"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                if e.kind() == ErrorKind::NotFound {
                    "No live X11 display (:0/:1) found and Xvfb is not installed (install with: sudo apt install xvfb)"
                        .to_owned()
                } else {
                    format!("failed to spawn Xvfb: {e}")
                }
            })?;

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut connected = None;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("Xvfb exited immediately with status {status}"));
            }
            if let Ok((c, s)) = RustConnection::connect(Some(FALLBACK_DISPLAY)) {
                connected = Some((c, s));
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        match connected {
            Some((c, s)) => (c, s, FALLBACK_DISPLAY.to_owned(), None, Some(child)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out waiting for Xvfb on {FALLBACK_DISPLAY}"));
            }
        }
    };

    let root = conn.setup().roots[screen_num].root;

    // Initialize required X11 extensions (Damage, Composite, XTEST).
    let _ = damage::query_version(&conn, 1, 1).ok().and_then(|c| c.reply().ok());
    let _ = composite::query_version(&conn, 0, 4).ok().and_then(|c| c.reply().ok());
    let _ = xtest::get_version(&conn, 2, 2).ok().and_then(|c| c.reply().ok());
    let _ = composite::redirect_subwindows(&conn, root, composite::Redirect::AUTOMATIC);

    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
    )
    .map_err(|e| format!("X11 SubstructureNotify subscribe failed: {e}"))?;

    let intern = |name: &[u8]| -> Atom {
        conn.intern_atom(false, name)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom)
            .unwrap_or(0)
    };
    let atoms = Atoms {
        net_wm_name: intern(b"_NET_WM_NAME"),
        utf8_string: intern(b"UTF8_STRING"),
        wm_protocols: intern(b"WM_PROTOCOLS"),
        wm_delete_window: intern(b"WM_DELETE_WINDOW"),
    };

    let (keymap, shift_keycode) = build_keymap(&conn);
    let _ = conn.flush();

    Ok(X11State {
        conn,
        display_name,
        xauthority,
        root,
        xvfb,
        atoms,
        windows: HashMap::new(),
        damage_to_win: HashMap::new(),
        keymap,
        shift_keycode,
        shift_held: false,
        focused_win: None,
    })
}

fn spawn_gui_command(command: &str, display_name: &str, xauthority: Option<&Path>) -> Result<(), String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err("empty GUI command".into());
    }
    log!("launching GUI app on {display_name}: {trimmed}");
    let mut cmd = Command::new("sh");
    cmd.args(["-c", trimmed])
        .env("DISPLAY", display_name)
        .env("XDG_SESSION_TYPE", "x11")
        .env("GDK_BACKEND", "x11")
        .env("QT_QPA_PLATFORM", "xcb")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(xa) = xauthority {
        cmd.env("XAUTHORITY", xa);
    }
    cmd.spawn().map_err(|e| format!("failed to launch `{trimmed}`: {e}"))?;
    Ok(())
}

fn build_keymap(conn: &RustConnection) -> (HashMap<u32, (u8, bool)>, Option<u8>) {
    let min_kc = conn.setup().min_keycode;
    let max_kc = conn.setup().max_keycode;
    let count = max_kc.saturating_sub(min_kc).saturating_add(1);
    let mut map = HashMap::new();
    let mut shift_kc = None;

    if let Ok(cookie) = conn.get_keyboard_mapping(min_kc, count)
        && let Ok(reply) = cookie.reply()
    {
        let per = reply.keysyms_per_keycode as usize;
        if per > 0 {
            for (idx, chunk) in reply.keysyms.chunks(per).enumerate() {
                let kc = min_kc.saturating_add(idx as u8);
                if let Some(&ks0) = chunk.first()
                    && ks0 != 0
                {
                    if ks0 == XK_SHIFT_L {
                        shift_kc = Some(kc);
                    }
                    map.entry(ks0).or_insert((kc, false));
                }
                if let Some(&ks1) = chunk.get(1)
                    && ks1 != 0
                {
                    map.entry(ks1).or_insert((kc, true));
                }
            }
        }
    }
    (map, shift_kc)
}

impl X11State {
    /// Attach an XDamage monitor to the active display's root window so the
    /// entire live desktop streams into a `pane` `[APP]` window.
    fn attach_root_desktop(&mut self, outbox: &Outbox) -> Result<(), String> {
        let root = self.root;
        let geom = self
            .conn
            .get_geometry(root)
            .ok()
            .and_then(|c| c.reply().ok())
            .ok_or_else(|| format!("failed to query root geometry on {}", self.display_name))?;
        let title = format!("Live Desktop ({})", self.display_name);

        if let Some(tw) = self.windows.get_mut(&root) {
            tw.active = true;
            tw.width = geom.width;
            tw.height = geom.height;
            tw.dirty = Some(DirtyRect { x: 0, y: 0, w: geom.width, h: geom.height });
            outbox.send(HostMsg::GuiWindowOpened {
                win_id: root,
                title,
                width: geom.width,
                height: geom.height,
            });
            return Ok(());
        }

        let damage_id = self.conn.generate_id().map_err(|e| format!("X11 generate_id failed: {e}"))?;
        damage::create(&self.conn, damage_id, root, damage::ReportLevel::BOUNDING_BOX)
            .map_err(|e| format!("XDamage create on root window failed: {e}"))?;
        let _ = self.conn.flush();

        log!("attached Live Desktop root 0x{root:x} on {} ({}x{})", self.display_name, geom.width, geom.height);

        self.windows.insert(
            root,
            TrackedWindow {
                x: 0,
                y: 0,
                width: geom.width,
                height: geom.height,
                damage: damage_id,
                active: true,
                is_root: true,
                dirty: Some(DirtyRect { x: 0, y: 0, w: geom.width, h: geom.height }),
            },
        );
        self.damage_to_win.insert(damage_id, root);

        outbox.send(HostMsg::GuiWindowOpened {
            win_id: root,
            title,
            width: geom.width,
            height: geom.height,
        });
        Ok(())
    }

    fn poll_events(&mut self, outbox: &Outbox) {
        while let Ok(Some(ev)) = self.conn.poll_for_event() {
            match ev {
                Event::MapNotify(m) => self.on_map_notify(m.window, m.override_redirect, outbox),
                Event::UnmapNotify(u) => self.on_window_gone(u.window, outbox),
                Event::DestroyNotify(d) => self.on_window_gone(d.window, outbox),
                Event::ConfigureNotify(c) => self.on_configure_notify(c.window, c.x, c.y, c.width, c.height, outbox),
                Event::DamageNotify(d) => self.on_damage_notify(d),
                _ => {}
            }
        }
        let _ = self.conn.flush();
    }

    fn on_map_notify(&mut self, win: Window, override_redirect: bool, outbox: &Outbox) {
        if win == self.root || self.windows.contains_key(&win) {
            return;
        }
        if override_redirect {
            let parent = self
                .conn
                .query_tree(win)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.parent)
                .unwrap_or(self.root);
            if parent == self.root {
                return;
            }
        }

        let Some(geom) = self.conn.get_geometry(win).ok().and_then(|c| c.reply().ok()) else {
            return;
        };
        if geom.width < 16 || geom.height < 16 {
            return;
        }

        // Only reposition negative-coordinate windows on our private Xvfb.
        let mut x = geom.x;
        let mut y = geom.y;
        if self.xvfb.is_some() && (x < 0 || y < 0) {
            x = x.max(0);
            y = y.max(0);
            let _ = self.conn.configure_window(
                win,
                &ConfigureWindowAux::new().x(i32::from(x)).y(i32::from(y)),
            );
        }

        let Ok(damage_id) = self.conn.generate_id() else {
            return;
        };
        if damage::create(&self.conn, damage_id, win, damage::ReportLevel::BOUNDING_BOX).is_err() {
            return;
        }
        let _ = self.conn.flush();

        let title = self.read_window_title(win);
        log!("mapped GUI window 0x{win:x} {:?} ({}x{} at {},{})", title, geom.width, geom.height, x, y);

        self.windows.insert(
            win,
            TrackedWindow {
                x,
                y,
                width: geom.width,
                height: geom.height,
                damage: damage_id,
                active: true,
                is_root: false,
                dirty: Some(DirtyRect { x: 0, y: 0, w: geom.width, h: geom.height }),
            },
        );
        self.damage_to_win.insert(damage_id, win);

        outbox.send(HostMsg::GuiWindowOpened {
            win_id: win,
            title,
            width: geom.width,
            height: geom.height,
        });
    }

    fn on_configure_notify(&mut self, win: Window, x: i16, y: i16, width: u16, height: u16, outbox: &Outbox) {
        let Some(tw) = self.windows.get_mut(&win) else {
            return;
        };
        if !tw.is_root {
            tw.x = x;
            tw.y = y;
        }
        if (tw.width, tw.height) != (width, height) && width > 0 && height > 0 {
            tw.width = width;
            tw.height = height;
            tw.dirty = Some(DirtyRect { x: 0, y: 0, w: width, h: height });
            outbox.send(HostMsg::GuiWindowResized { win_id: win, width, height });
        }
    }

    fn on_window_gone(&mut self, win: Window, outbox: &Outbox) {
        if win == self.root {
            return;
        }
        if let Some(tw) = self.windows.remove(&win) {
            self.damage_to_win.remove(&tw.damage);
            let _ = damage::destroy(&self.conn, tw.damage);
            if self.focused_win == Some(win) {
                self.focused_win = None;
            }
            log!("closed GUI window 0x{win:x}");
            outbox.send(HostMsg::GuiWindowClosed { win_id: win });
        }
    }

    fn on_damage_notify(&mut self, ev: damage::NotifyEvent) {
        let _ = damage::subtract(&self.conn, ev.damage, x11rb::NONE, x11rb::NONE);
        let Some(&win) = self.damage_to_win.get(&ev.damage) else {
            return;
        };
        let Some(tw) = self.windows.get_mut(&win) else {
            return;
        };
        let rect = DirtyRect {
            x: ev.area.x.max(0) as u16,
            y: ev.area.y.max(0) as u16,
            w: ev.area.width,
            h: ev.area.height,
        };
        tw.dirty = Some(match tw.dirty {
            Some(existing) => existing.union(rect),
            None => rect,
        });
    }

    fn flush_damage(&mut self, outbox: &Outbox) {
        let win_ids: Vec<Window> = self.windows.keys().copied().collect();
        for win_id in win_ids {
            let Some(tw) = self.windows.get_mut(&win_id) else {
                continue;
            };
            if !tw.active {
                continue;
            }
            let Some(raw_dirty) = tw.dirty.take() else {
                continue;
            };
            let Some(d) = raw_dirty.clamp(tw.width, tw.height) else {
                continue;
            };

            let Ok(cookie) = self.conn.get_image(
                ImageFormat::Z_PIXMAP,
                win_id,
                d.x as i16,
                d.y as i16,
                d.w,
                d.h,
                u32::MAX,
            ) else {
                continue;
            };
            let Ok(reply) = cookie.reply() else {
                continue;
            };
            let expected = (d.w as usize) * (d.h as usize) * 4;
            if reply.data.len() < expected {
                continue;
            }
            let rgba = bgra_to_rgba(&reply.data[..expected]);
            let rgba_lz4 = lz4_flex::compress_prepend_size(&rgba);

            let _ = outbox.send_bulk(HostMsg::GuiWindowDamage {
                win_id,
                x: d.x,
                y: d.y,
                width: d.w,
                height: d.h,
                rgba_lz4,
            });
        }
    }

    fn read_window_title(&self, win: Window) -> String {
        if self.atoms.net_wm_name != 0
            && let Ok(c) = self.conn.get_property(false, win, self.atoms.net_wm_name, self.atoms.utf8_string, 0, 256)
            && let Ok(r) = c.reply()
            && !r.value.is_empty()
        {
            let s = String::from_utf8_lossy(&r.value).trim().to_owned();
            if !s.is_empty() {
                return s;
            }
        }
        if let Ok(c) = self.conn.get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 256)
            && let Ok(r) = c.reply()
            && !r.value.is_empty()
        {
            let s = String::from_utf8_lossy(&r.value).trim().to_owned();
            if !s.is_empty() {
                return s;
            }
        }
        format!("GUI App 0x{win:x}")
    }

    fn to_root_coords(&self, win_id: Window, x: i16, y: i16) -> Option<(i16, i16)> {
        let tw = self.windows.get(&win_id)?;
        Some((tw.x.saturating_add(x), tw.y.saturating_add(y)))
    }

    fn focus_window(&mut self, win_id: Window) {
        if win_id == self.root {
            return;
        }
        if self.focused_win != Some(win_id) {
            let _ = self.conn.configure_window(win_id, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
            let _ = self.conn.set_input_focus(InputFocus::PARENT, win_id, x11rb::CURRENT_TIME);
            self.focused_win = Some(win_id);
        }
    }

    fn handle_mouse_move(&mut self, win_id: Window, x: i16, y: i16) {
        let Some((rx, ry)) = self.to_root_coords(win_id, x, y) else {
            return;
        };
        let _ = xtest::fake_input(&self.conn, xproto::MOTION_NOTIFY_EVENT, 0, 0, self.root, rx, ry, 0);
        let _ = self.conn.flush();
    }

    fn handle_mouse_button(&mut self, win_id: Window, button: u8, pressed: bool, x: i16, y: i16) {
        let Some((rx, ry)) = self.to_root_coords(win_id, x, y) else {
            return;
        };
        if pressed {
            self.focus_window(win_id);
        }
        let _ = xtest::fake_input(&self.conn, xproto::MOTION_NOTIFY_EVENT, 0, 0, self.root, rx, ry, 0);
        let ev_type = if pressed { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        let _ = xtest::fake_input(&self.conn, ev_type, button, 0, self.root, rx, ry, 0);
        let _ = self.conn.flush();
    }

    fn handle_scroll(&mut self, win_id: Window, delta_y: i16, x: i16, y: i16) {
        let Some((rx, ry)) = self.to_root_coords(win_id, x, y) else {
            return;
        };
        if delta_y == 0 {
            return;
        }
        let button: u8 = if delta_y > 0 { 4 } else { 5 };
        let steps = (delta_y.unsigned_abs() / 16).clamp(1, 5);
        let _ = xtest::fake_input(&self.conn, xproto::MOTION_NOTIFY_EVENT, 0, 0, self.root, rx, ry, 0);
        for _ in 0..steps {
            let _ = xtest::fake_input(&self.conn, xproto::BUTTON_PRESS_EVENT, button, 0, self.root, rx, ry, 0);
            let _ = xtest::fake_input(&self.conn, xproto::BUTTON_RELEASE_EVENT, button, 0, self.root, rx, ry, 0);
        }
        let _ = self.conn.flush();
    }

    fn handle_key(&mut self, win_id: Window, keysym: u32, pressed: bool) {
        if !self.windows.contains_key(&win_id) {
            return;
        }
        if pressed {
            self.focus_window(win_id);
        }
        if keysym == XK_SHIFT_L || keysym == XK_SHIFT_R {
            self.shift_held = pressed;
        }
        let Some(&(keycode, needs_shift)) = self.keymap.get(&keysym) else {
            return;
        };
        let synth_shift = needs_shift && !self.shift_held;
        let ev_type = if pressed { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };

        if synth_shift
            && pressed
            && let Some(skc) = self.shift_keycode
        {
            let _ = xtest::fake_input(&self.conn, xproto::KEY_PRESS_EVENT, skc, 0, self.root, 0, 0, 0);
        }
        let _ = xtest::fake_input(&self.conn, ev_type, keycode, 0, self.root, 0, 0, 0);
        if synth_shift
            && !pressed
            && let Some(skc) = self.shift_keycode
        {
            let _ = xtest::fake_input(&self.conn, xproto::KEY_RELEASE_EVENT, skc, 0, self.root, 0, 0, 0);
        }
        let _ = self.conn.flush();
    }

    fn handle_close_window(&mut self, win_id: Window, outbox: &Outbox) {
        let Some(tw) = self.windows.get(&win_id) else {
            return;
        };
        // Closing the Live Desktop stream detaches XDamage without disturbing the remote desktop.
        if tw.is_root {
            if let Some(removed) = self.windows.remove(&win_id) {
                self.damage_to_win.remove(&removed.damage);
                let _ = damage::destroy(&self.conn, removed.damage);
                let _ = self.conn.flush();
                log!("detached Live Desktop root stream on {}", self.display_name);
                outbox.send(HostMsg::GuiWindowClosed { win_id });
            }
            return;
        }

        // Prefer graceful WM_DELETE_WINDOW if the client advertises it.
        let supports_delete = self.atoms.wm_protocols != 0
            && self.atoms.wm_delete_window != 0
            && self
                .conn
                .get_property(false, win_id, self.atoms.wm_protocols, AtomEnum::ATOM, 0, 32)
                .ok()
                .and_then(|c| c.reply().ok())
                .and_then(|r| r.value32().map(|mut it| it.any(|a| a == self.atoms.wm_delete_window)))
                .unwrap_or(false);

        if supports_delete {
            let msg = ClientMessageEvent {
                response_type: xproto::CLIENT_MESSAGE_EVENT,
                format: 32,
                sequence: 0,
                window: win_id,
                type_: self.atoms.wm_protocols,
                data: ClientMessageData::from([self.atoms.wm_delete_window, x11rb::CURRENT_TIME, 0, 0, 0]),
            };
            let _ = self.conn.send_event(false, win_id, EventMask::NO_EVENT, msg);
        } else {
            let _ = self.conn.destroy_window(win_id);
        }
        let _ = self.conn.flush();
    }
}

/// Convert X11 Z_PIXMAP BGRA/BGRX bytes into opaque RGBA pixels.
pub fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bgra.len());
    for px in bgra.chunks_exact(4) {
        out.push(px[2]); // R
        out.push(px[1]); // G
        out.push(px[0]); // B
        out.push(255); // A
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_converts_to_opaque_rgba() {
        let bgra = [10u8, 20, 30, 0, 255, 128, 0, 42];
        assert_eq!(bgra_to_rgba(&bgra), vec![30, 20, 10, 255, 0, 128, 255, 255]);
    }

    #[test]
    fn dirty_rect_union_and_clamp() {
        let a = DirtyRect { x: 10, y: 20, w: 30, h: 40 };
        let b = DirtyRect { x: 25, y: 10, w: 50, h: 30 };
        let u = a.union(b);
        assert_eq!(u, DirtyRect { x: 10, y: 10, w: 65, h: 50 });
        assert_eq!(u.clamp(50, 40), Some(DirtyRect { x: 10, y: 10, w: 40, h: 30 }));
        assert_eq!(DirtyRect { x: 100, y: 0, w: 10, h: 10 }.clamp(50, 50), None);
    }
}
