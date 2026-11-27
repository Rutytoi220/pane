//! Wayland input emulation (via `reis` / `libei` over `org.kde.KWin.EIS.RemoteDesktop`
//! or `org.freedesktop.portal.RemoteDesktop`) and full-desktop screen streaming
//! (via `org.freedesktop.portal.ScreenCast` + PipeWire `gst-launch-1.0`).
//!
//! 100% pure-Rust D-Bus (`zbus`) + `libei` (`reis`) with zero C library linkage,
//! preserving static `x86_64-unknown-linux-musl` builds.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_core::Stream;
use pane_proto::HostMsg;
use reis::ei;
use reis::event::{Device, DeviceCapability, EiEvent};
use zbus::names::InterfaceName;
use zbus::{Connection, OwnedObjectPath, OwnedVariant, Proxy, Variant};

use crate::outbox::Outbox;

/// Reserved window ID for the full Wayland desktop stream.
pub const WAYLAND_DESKTOP_WIN_ID: u32 = 0xffff_fffe;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;
const EVDEV_KEY_LEFTSHIFT: u32 = 42;

/// Ensure `XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS` point to the active
/// desktop user's session bus when launched from a bare SSH session.
pub fn ensure_session_env() {
    let uid = unsafe { libc::geteuid() };
    let runtime_dir = format!("/run/user/{uid}");
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() && Path::new(&runtime_dir).is_dir() {
        // SAFETY: called on the single gui-bridge thread before spawning helpers.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir) };
    }
    let bus_sock = format!("{runtime_dir}/bus");
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() && Path::new(&bus_sock).exists() {
        unsafe { std::env::set_var("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={bus_sock}")) };
    }
}

/// True if a Wayland compositor socket exists in `/run/user/<uid>/`.
pub fn has_wayland_session() -> bool {
    let uid = unsafe { libc::geteuid() };
    let dir = PathBuf::from(format!("/run/user/{uid}"));
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if s.starts_with("wayland-") && !s.ends_with(".lock") {
                return true;
            }
        }
    }
    false
}

#[derive(Default)]
struct EiDeviceSet {
    abs_dev: Option<Device>,
    abs_iface: Option<ei::PointerAbsolute>,
    btn_iface: Option<ei::Button>,
    scroll_iface: Option<ei::Scroll>,
    kbd_dev: Option<Device>,
    kbd_iface: Option<ei::Keyboard>,
    pointer_dev: Option<Device>,
}

/// Real-time Wayland input injector using `libei` (`reis`).
pub struct EiInjector {
    conn: reis::event::Connection,
    devices: Arc<Mutex<EiDeviceSet>>,
    seq: AtomicU32,
    alive: Arc<AtomicBool>,
}

impl EiInjector {
    /// Connect directly to KWin's EIS server (`org.kde.KWin.EIS.RemoteDesktop`),
    /// which requires zero user consent dialogs on KDE Plasma Wayland.
    pub fn connect_kwin() -> Result<Self, String> {
        ensure_session_env();
        let fd: std::os::fd::OwnedFd = zbus::block_on(async {
            let dbus = Connection::session().await.map_err(|e| format!("session dbus: {e}"))?;
            let reply = dbus
                .call_method(
                    Some("org.kde.KWin"),
                    "/org/kde/KWin/EIS/RemoteDesktop",
                    Some("org.kde.KWin.EIS.RemoteDesktop"),
                    "connectToEIS",
                    &(7i32,),
                )
                .await
                .map_err(|e| format!("KWin connectToEIS: {e}"))?;
            let (fd, _cookie): (zbus::OwnedFd, i32) =
                reply.body().deserialize().map_err(|e| format!("decode EIS fd: {e}"))?;
            Ok::<_, String>(std::os::fd::OwnedFd::from(fd))
        })?;
        Self::from_fd(fd)
    }

    pub fn from_fd(fd: std::os::fd::OwnedFd) -> Result<Self, String> {
        let stream = UnixStream::from(fd);
        let devices = Arc::new(Mutex::new(EiDeviceSet::default()));
        let alive = Arc::new(AtomicBool::new(true));
        let devices_bg = Arc::clone(&devices);
        let alive_bg = Arc::clone(&alive);
        let (init_tx, init_rx) = crossbeam_channel::bounded::<Result<reis::event::Connection, String>>(1);

        thread::Builder::new()
            .name("ei-events".into())
            .spawn(move || {
                let ctx = match ei::Context::new(stream) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = init_tx.send(Err(format!("ei context: {e}")));
                        return;
                    }
                };
                let (conn, events) = match ctx.handshake_blocking("pane-host", ei::handshake::ContextType::Sender) {
                    Ok(pair) => pair,
                    Err(e) => {
                        let _ = init_tx.send(Err(format!("ei handshake: {e}")));
                        return;
                    }
                };
                let _ = init_tx.send(Ok(conn));

                for ev in events {
                    match ev {
                        Ok(EiEvent::SeatAdded(s)) => {
                            s.seat.bind_capabilities(
                                DeviceCapability::Pointer
                                    | DeviceCapability::PointerAbsolute
                                    | DeviceCapability::Keyboard
                                    | DeviceCapability::Scroll
                                    | DeviceCapability::Button,
                            );
                            let _ = ctx.flush();
                        }
                        Ok(EiEvent::DeviceAdded(d)) => register_device(&devices_bg, &d.device),
                        Ok(EiEvent::DeviceResumed(d)) => register_device(&devices_bg, &d.device),
                        Ok(EiEvent::DeviceRemoved(d)) => unregister_device(&devices_bg, &d.device),
                        Ok(EiEvent::Disconnected(_)) | Err(_) => break,
                        _ => {}
                    }
                }
                alive_bg.store(false, Ordering::Relaxed);
            })
            .map_err(|e| format!("spawn ei thread: {e}"))?;

        let conn = init_rx
            .recv()
            .map_err(|_| "ei event thread exited before handshake".to_owned())??;

        log!("connected Wayland EIS input injector");
        Ok(Self { conn, devices, seq: AtomicU32::new(1), alive })
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn next_seq(&self) -> u32 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    fn now_us() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0)
    }

    pub fn mouse_move(&self, x: f32, y: f32) {
        let Ok(devs) = self.devices.lock() else { return };
        let Some(dev) = devs.abs_dev.as_ref().or(devs.pointer_dev.as_ref()) else {
            return;
        };
        let Some(abs) = devs.abs_iface.as_ref() else {
            return;
        };
        let serial = self.conn.serial();
        let seq = self.next_seq();
        dev.device().start_emulating(serial, seq);
        abs.motion_absolute(x, y);
        dev.device().frame(serial, Self::now_us());
        dev.device().stop_emulating(serial);
        let _ = self.conn.flush();
    }

    pub fn mouse_button(&self, button: u8, pressed: bool, x: f32, y: f32) {
        let Ok(devs) = self.devices.lock() else { return };
        let Some(dev) = devs.abs_dev.as_ref().or(devs.pointer_dev.as_ref()) else {
            return;
        };
        let Some(btn) = devs.btn_iface.as_ref() else {
            return;
        };
        let evdev_btn = match button {
            1 => BTN_LEFT,
            2 => BTN_MIDDLE,
            3 => BTN_RIGHT,
            8 => BTN_SIDE,
            9 => BTN_EXTRA,
            _ => BTN_LEFT,
        };
        let state = if pressed {
            ei::button::ButtonState::Press
        } else {
            ei::button::ButtonState::Released
        };
        let serial = self.conn.serial();
        let seq = self.next_seq();
        dev.device().start_emulating(serial, seq);
        if let Some(abs) = devs.abs_iface.as_ref() {
            abs.motion_absolute(x, y);
        }
        btn.button(evdev_btn, state);
        dev.device().frame(serial, Self::now_us());
        dev.device().stop_emulating(serial);
        let _ = self.conn.flush();
    }

    pub fn scroll(&self, delta_y: i16, x: f32, y: f32) {
        if delta_y == 0 {
            return;
        }
        let Ok(devs) = self.devices.lock() else { return };
        let Some(dev) = devs.abs_dev.as_ref().or(devs.pointer_dev.as_ref()) else {
            return;
        };
        let Some(scroll) = devs.scroll_iface.as_ref() else {
            return;
        };
        let serial = self.conn.serial();
        let seq = self.next_seq();
        dev.device().start_emulating(serial, seq);
        if let Some(abs) = devs.abs_iface.as_ref() {
            abs.motion_absolute(x, y);
        }
        // In egui positive delta_y is scroll UP; in libei/Wayland negative Y is scroll UP.
        let discrete = if delta_y > 0 { -120 } else { 120 };
        scroll.scroll_discrete(0, discrete);
        dev.device().frame(serial, Self::now_us());
        dev.device().stop_emulating(serial);
        let _ = self.conn.flush();
    }

    pub fn key(&self, keysym: u32, pressed: bool) {
        let Some((evdev_code, needs_shift)) = keysym_to_evdev(keysym) else {
            return;
        };
        let Ok(devs) = self.devices.lock() else { return };
        let Some(dev) = devs.kbd_dev.as_ref() else {
            return;
        };
        let Some(kbd) = devs.kbd_iface.as_ref() else {
            return;
        };
        let serial = self.conn.serial();
        let seq = self.next_seq();
        let state = if pressed {
            ei::keyboard::KeyState::Press
        } else {
            ei::keyboard::KeyState::Released
        };
        dev.device().start_emulating(serial, seq);
        if needs_shift && pressed {
            kbd.key(EVDEV_KEY_LEFTSHIFT, ei::keyboard::KeyState::Press);
        }
        kbd.key(evdev_code, state);
        if needs_shift && !pressed {
            kbd.key(EVDEV_KEY_LEFTSHIFT, ei::keyboard::KeyState::Released);
        }
        dev.device().frame(serial, Self::now_us());
        dev.device().stop_emulating(serial);
        let _ = self.conn.flush();
    }
}

fn register_device(devices: &Arc<Mutex<EiDeviceSet>>, dev: &Device) {
    let Ok(mut set) = devices.lock() else { return };
    if let Some(abs) = dev.interface::<ei::PointerAbsolute>() {
        set.abs_dev = Some(dev.clone());
        set.abs_iface = Some(abs);
    }
    if let Some(btn) = dev.interface::<ei::Button>() {
        set.pointer_dev = Some(dev.clone());
        set.btn_iface = Some(btn);
    }
    if let Some(scroll) = dev.interface::<ei::Scroll>() {
        set.pointer_dev = Some(dev.clone());
        set.scroll_iface = Some(scroll);
    }
    if let Some(kbd) = dev.interface::<ei::Keyboard>() {
        set.kbd_dev = Some(dev.clone());
        set.kbd_iface = Some(kbd);
    }
}

fn unregister_device(devices: &Arc<Mutex<EiDeviceSet>>, dev: &Device) {
    let Ok(mut set) = devices.lock() else { return };
    if set.abs_dev.as_ref() == Some(dev) {
        set.abs_dev = None;
        set.abs_iface = None;
    }
    if set.pointer_dev.as_ref() == Some(dev) {
        set.pointer_dev = None;
        set.btn_iface = None;
        set.scroll_iface = None;
    }
    if set.kbd_dev.as_ref() == Some(dev) {
        set.kbd_dev = None;
        set.kbd_iface = None;
    }
}

/// Active background handle for the Wayland full-desktop PipeWire stream.
pub struct WaylandDesktopStream {
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl WaylandDesktopStream {
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Start a Wayland `org.freedesktop.portal.ScreenCast` + `gst-launch-1.0`
/// PipeWire desktop stream on a background thread.
pub fn spawn_desktop_stream(req_id: u64, outbox: Outbox) -> WaylandDesktopStream {
    let active = Arc::new(AtomicBool::new(true));
    let stop = Arc::new(AtomicBool::new(false));
    let active_bg = Arc::clone(&active);
    let stop_bg = Arc::clone(&stop);

    let handle = thread::Builder::new()
        .name("wayland-screencast".into())
        .spawn(move || {
            ensure_session_env();
            if let Err(msg) = run_screencast_loop(active_bg, stop_bg, &outbox) {
                log!("wayland desktop stream stopped: {msg}");
                outbox.send(HostMsg::Error { req_id: Some(req_id), message: msg });
            }
            outbox.send(HostMsg::GuiWindowClosed { win_id: WAYLAND_DESKTOP_WIN_ID });
        })
        .expect("spawn wayland screencast");

    WaylandDesktopStream { active, stop, handle: Some(handle) }
}

fn token_path() -> PathBuf {
    crate::files::home_dir().join(".cache/pane/screencast.token")
}

fn run_screencast_loop(
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    outbox: &Outbox,
) -> Result<(), String> {
    let (node_id, width, height, pw_fd) = zbus::block_on(open_portal_screencast())?;

    log!("starting PipeWire Wayland desktop capture: node={node_id} ({width}x{height})");

    let raw_pw_fd = pw_fd.as_raw_fd();
    let mut cmd = Command::new("gst-launch-1.0");
    cmd.args([
        "-q",
        "pipewiresrc",
        "fd=3",
        &format!("path={node_id}"),
        "do-timestamp=true",
        "keepalive-time=1000",
        "!",
        "videoconvert",
        "!",
        &format!("video/x-raw,format=RGBA,width={width},height={height}"),
        "!",
        "fdsink",
        "fd=1",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    // SAFETY: dup2 the PipeWire remote socket onto fd 3 in the child before exec.
    unsafe {
        cmd.pre_exec(move || {
            if raw_pw_fd != 3 {
                if libc::dup2(raw_pw_fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else {
                libc::fcntl(3, libc::F_SETFD, 0);
            }
            Ok(())
        });
    }

    let mut child: Child = cmd.spawn().map_err(|e| {
        format!("failed to start gst-launch-1.0 for PipeWire desktop capture: {e}")
    })?;
    drop(pw_fd);

    let mut stdout = child.stdout.take().ok_or_else(|| "gst-launch stdout missing".to_owned())?;
    outbox.send(HostMsg::GuiWindowOpened {
        win_id: WAYLAND_DESKTOP_WIN_ID,
        title: "Live Desktop (Wayland)".into(),
        width,
        height,
    });

    let frame_bytes = (width as usize) * (height as usize) * 4;
    let mut prev_frame = vec![0u8; frame_bytes];
    let mut cur_frame = vec![0u8; frame_bytes];
    let mut have_first = false;
    let mut last_sent = Instant::now() - Duration::from_secs(1);

    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = read_exact_or_eof(&mut stdout, &mut cur_frame) {
            log!("PipeWire gst stream ended: {e}");
            break;
        }
        if !active.load(Ordering::Relaxed) {
            continue;
        }
        if last_sent.elapsed() < Duration::from_millis(33) {
            continue;
        }

        if !have_first {
            let rgba_lz4 = lz4_flex::compress_prepend_size(&cur_frame);
            if !outbox.send_bulk(HostMsg::GuiWindowDamage {
                win_id: WAYLAND_DESKTOP_WIN_ID,
                x: 0,
                y: 0,
                width,
                height,
                rgba_lz4,
            }) {
                break;
            }
            prev_frame.copy_from_slice(&cur_frame);
            have_first = true;
            last_sent = Instant::now();
            continue;
        }

        if let Some((rx, ry, rw, rh)) = compute_rgba_diff_rect(&prev_frame, &cur_frame, width, height) {
            let patch = extract_rgba_subrect(&cur_frame, width, rx, ry, rw, rh);
            let rgba_lz4 = lz4_flex::compress_prepend_size(&patch);
            if !outbox.send_bulk(HostMsg::GuiWindowDamage {
                win_id: WAYLAND_DESKTOP_WIN_ID,
                x: rx,
                y: ry,
                width: rw,
                height: rh,
                rgba_lz4,
            }) {
                break;
            }
            prev_frame.copy_from_slice(&cur_frame);
            last_sent = Instant::now();
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof")),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Call `org.freedesktop.portal.ScreenCast` (`CreateSession` -> `SelectSources` ->
/// `Start` -> `OpenPipeWireRemote`), caching `restore_token` in `~/.cache/pane/`.
async fn open_portal_screencast() -> Result<(u32, u16, u16, OwnedFd), String> {
    let conn = Connection::session().await.map_err(|e| format!("D-Bus session error: {e}"))?;
    let sc = Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
    )
    .await
    .map_err(|e| format!("ScreenCast portal proxy: {e}"))?;

    let token_seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(1);

    // 1. CreateSession
    let mut opts: HashMap<&str, Variant<'_>> = HashMap::new();
    let handle_token1 = format!("pane_cs_{token_seed}");
    let session_token = format!("pane_sess_{token_seed}");
    opts.insert("handle_token", Variant::from(handle_token1.as_str()));
    opts.insert("session_handle_token", Variant::from(session_token.as_str()));

    let results = portal_call(&conn, &sc, "CreateSession", &handle_token1, &(opts,)).await?;
    let session_path_str = match results.get("session_handle") {
        Some(v) => v.downcast_ref::<zbus::ObjectPath<'_>>().map(|p| p.to_string())
            .or_else(|_| v.downcast_ref::<String>())
            .map_err(|e| format!("invalid session_handle: {e}"))?,
        None => return Err("ScreenCast CreateSession missing session_handle".into()),
    };
    let session_path = OwnedObjectPath::try_from(session_path_str).map_err(|e| format!("session path: {e}"))?;

    // 2. SelectSources (Monitor, embedded cursor, persist_mode = 2).
    let mut sel_opts: HashMap<&str, Variant<'_>> = HashMap::new();
    let handle_token2 = format!("pane_ss_{token_seed}");
    sel_opts.insert("handle_token", Variant::from(handle_token2.as_str()));
    sel_opts.insert("types", Variant::from(1u32)); // MONITOR
    sel_opts.insert("multiple", Variant::from(false));
    sel_opts.insert("cursor_mode", Variant::from(2u32)); // EMBEDDED
    sel_opts.insert("persist_mode", Variant::from(2u32)); // PERSIST UNTIL REVOKED
    let cached_token = fs::read_to_string(token_path()).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    if let Some(ref tok) = cached_token {
        sel_opts.insert("restore_token", Variant::from(tok.as_str()));
    }
    let _ = portal_call(&conn, &sc, "SelectSources", &handle_token2, &(&session_path, sel_opts)).await?;

    // 3. Start (may prompt once on remote desktop if no restore_token was saved yet).
    let mut start_opts: HashMap<&str, Variant<'_>> = HashMap::new();
    let handle_token3 = format!("pane_st_{token_seed}");
    start_opts.insert("handle_token", Variant::from(handle_token3.as_str()));
    let start_res = portal_call(&conn, &sc, "Start", &handle_token3, &(&session_path, "", start_opts)).await?;

    if let Some(tok_val) = start_res.get("restore_token")
        && let Ok(tok) = tok_val.downcast_ref::<String>()
        && !tok.is_empty()
    {
        let p = token_path();
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(p, tok);
    }

    let streams_val = start_res
        .get("streams")
        .ok_or_else(|| "ScreenCast Start returned no streams".to_owned())?;
    let streams_arr = streams_val
        .downcast_ref::<zbus::wire::Array<'_>>()
        .map_err(|e| format!("decode ScreenCast streams array: {e}"))?;
    let first_stream = streams_arr
        .first()
        .ok_or_else(|| "ScreenCast Start returned empty stream list".to_owned())?;
    let stream_struct = first_stream
        .downcast_ref::<zbus::wire::Structure<'_>>()
        .map_err(|e| format!("decode ScreenCast stream tuple: {e}"))?;
    let fields = stream_struct.fields();
    let node_id: u32 = fields
        .first()
        .ok_or_else(|| "ScreenCast stream missing node_id".to_owned())?
        .downcast_ref::<u32>()
        .map_err(|e| format!("decode ScreenCast node_id: {e}"))?;
    let (width, height) = fields
        .get(1)
        .and_then(|v| v.downcast_ref::<zbus::wire::Dict<'_, '_>>().ok())
        .and_then(|dict| {
            let size_val = dict.get::<&str, Variant<'_>>(&"size").ok().flatten()?;
            let (w, h) = size_val.downcast_ref::<(i32, i32)>().ok()?;
            Some(((w.max(1)) as u16, (h.max(1)) as u16))
        })
        .unwrap_or((1680, 1050));

    // 4. OpenPipeWireRemote
    let empty_opts: HashMap<&str, Variant<'_>> = HashMap::new();
    let reply = sc
        .call_method("OpenPipeWireRemote", &(&session_path, empty_opts))
        .await
        .map_err(|e| format!("OpenPipeWireRemote: {e}"))?;
    let zfd: zbus::OwnedFd = reply.body().deserialize().map_err(|e| format!("decode PipeWire fd: {e}"))?;
    Ok((node_id, width, height, OwnedFd::from(zfd)))
}

async fn portal_call<B: serde::Serialize + zbus::wire::Type>(
    conn: &Connection,
    portal: &Proxy<'_>,
    method: &str,
    handle_token: &str,
    body: &B,
) -> Result<HashMap<String, OwnedVariant>, String> {
    // Subscribe to the deterministic Request::Response signal BEFORE calling the
    // portal method so fast synchronous responses cannot be missed.
    let sender = conn
        .unique_name()
        .ok_or_else(|| "D-Bus connection has no unique name".to_owned())?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    let expected_path = OwnedObjectPath::try_from(format!("/org/freedesktop/portal/desktop/request/{sender}/{handle_token}"))
        .map_err(|e| format!("request path: {e}"))?;
    let req_iface = InterfaceName::from_static_str("org.freedesktop.portal.Request").unwrap();
    let pre_proxy = Proxy::new(conn, "org.freedesktop.portal.Desktop", expected_path, req_iface.clone())
        .await
        .map_err(|e| format!("portal Request proxy: {e}"))?;
    let mut signals = pre_proxy
        .receive_signal("Response")
        .await
        .map_err(|e| format!("portal Request::Response signal: {e}"))?;

    let reply = portal
        .call_method(method, body)
        .await
        .map_err(|e| format!("portal {method}: {e}"))?;
    let actual_path: OwnedObjectPath = reply
        .body()
        .deserialize()
        .map_err(|e| format!("portal {method} request path: {e}"))?;
    if actual_path.as_ref() != *pre_proxy.path() {
        let actual_proxy = Proxy::new(conn, "org.freedesktop.portal.Desktop", actual_path, req_iface)
            .await
            .map_err(|e| format!("portal Request proxy: {e}"))?;
        signals = actual_proxy
            .receive_signal("Response")
            .await
            .map_err(|e| format!("portal Request::Response signal: {e}"))?;
    }

    let msg = std::future::poll_fn(|cx| Pin::new(&mut signals).poll_next(cx))
        .await
        .ok_or_else(|| format!("portal {method} closed without Response"))?;

    let (code, results): (u32, HashMap<String, OwnedVariant>) = msg
        .body()
        .deserialize()
        .map_err(|e| format!("portal {method} Response body: {e}"))?;

    if code != 0 {
        return Err(format!("Wayland ScreenCast {method} declined/cancelled (code {code})"));
    }
    Ok(results)
}

/// Compute the minimal bounding rectangle of pixels that changed between `prev`
/// and `curr`. Returns `None` when the frame is pixel-identical.
pub fn compute_rgba_diff_rect(prev: &[u8], curr: &[u8], width: u16, height: u16) -> Option<(u16, u16, u16, u16)> {
    let w = width as usize;
    let h = height as usize;
    let stride = w * 4;
    if prev.len() < stride * h || curr.len() < stride * h {
        return None;
    }

    let mut min_y = h;
    let mut max_y = 0usize;
    for y in 0..h {
        let row_slice = y * stride..(y + 1) * stride;
        if prev[row_slice.clone()] != curr[row_slice] {
            if min_y == h {
                min_y = y;
            }
            max_y = y;
        }
    }
    if min_y == h {
        return None;
    }

    let mut min_x = w;
    let mut max_x = 0usize;
    for y in min_y..=max_y {
        let base = y * stride;
        for x in 0..w {
            let off = base + x * 4;
            if prev[off..off + 4] != curr[off..off + 4] {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
            }
        }
    }

    if min_x > max_x {
        None
    } else {
        Some((
            min_x as u16,
            min_y as u16,
            (max_x - min_x + 1) as u16,
            (max_y - min_y + 1) as u16,
        ))
    }
}

fn extract_rgba_subrect(frame: &[u8], frame_w: u16, rx: u16, ry: u16, rw: u16, rh: u16) -> Vec<u8> {
    let stride = frame_w as usize * 4;
    let row_bytes = rw as usize * 4;
    let mut out = Vec::with_capacity(row_bytes * rh as usize);
    for dy in 0..rh as usize {
        let start = (ry as usize + dy) * stride + rx as usize * 4;
        out.extend_from_slice(&frame[start..start + row_bytes]);
    }
    out
}

/// Map standard X11 keysyms to Linux evdev keycodes (`<linux/input-event-codes.h>`)
/// along with whether `KEY_LEFTSHIFT` should be held.
pub fn keysym_to_evdev(ks: u32) -> Option<(u32, bool)> {
    let pair = match ks {
        // Letters (lowercase & uppercase)
        0x61 => (30, false), 0x41 => (30, true), // a / A
        0x62 => (48, false), 0x42 => (48, true), // b / B
        0x63 => (46, false), 0x43 => (46, true), // c / C
        0x64 => (32, false), 0x44 => (32, true), // d / D
        0x65 => (18, false), 0x45 => (18, true), // e / E
        0x66 => (33, false), 0x46 => (33, true), // f / F
        0x67 => (34, false), 0x47 => (34, true), // g / G
        0x68 => (35, false), 0x48 => (35, true), // h / H
        0x69 => (23, false), 0x49 => (23, true), // i / I
        0x6a => (36, false), 0x4a => (36, true), // j / J
        0x6b => (37, false), 0x4b => (37, true), // k / K
        0x6c => (38, false), 0x4c => (38, true), // l / L
        0x6d => (50, false), 0x4d => (50, true), // m / M
        0x6e => (49, false), 0x4e => (49, true), // n / N
        0x6f => (24, false), 0x4f => (24, true), // o / O
        0x70 => (25, false), 0x50 => (25, true), // p / P
        0x71 => (16, false), 0x51 => (16, true), // q / Q
        0x72 => (19, false), 0x52 => (19, true), // r / R
        0x73 => (31, false), 0x53 => (31, true), // s / S
        0x74 => (20, false), 0x54 => (20, true), // t / T
        0x75 => (22, false), 0x55 => (22, true), // u / U
        0x76 => (47, false), 0x56 => (47, true), // v / V
        0x77 => (17, false), 0x57 => (17, true), // w / W
        0x78 => (45, false), 0x58 => (45, true), // x / X
        0x79 => (21, false), 0x59 => (21, true), // y / Y
        0x7a => (44, false), 0x5a => (44, true), // z / Z
        // Digits & shifted US symbols
        0x31 => (2, false),  0x21 => (2, true),  // 1 / !
        0x32 => (3, false),  0x40 => (3, true),  // 2 / @
        0x33 => (4, false),  0x23 => (4, true),  // 3 / #
        0x34 => (5, false),  0x24 => (5, true),  // 4 / $
        0x35 => (6, false),  0x25 => (6, true),  // 5 / %
        0x36 => (7, false),  0x5e => (7, true),  // 6 / ^
        0x37 => (8, false),  0x26 => (8, true),  // 7 / &
        0x38 => (9, false),  0x2a => (9, true),  // 8 / *
        0x39 => (10, false), 0x28 => (10, true), // 9 / (
        0x30 => (11, false), 0x29 => (11, true), // 0 / )
        0x20 => (57, false),                     // space
        0x2d => (12, false), 0x5f => (12, true), // - / _
        0x3d => (13, false), 0x2b => (13, true), // = / +
        0x5b => (26, false), 0x7b => (26, true), // [ / {
        0x5d => (27, false), 0x7d => (27, true), // ] / }
        0x5c => (43, false), 0x7c => (43, true), // \ / |
        0x3b => (39, false), 0x3a => (39, true), // ; / :
        0x27 => (40, false), 0x22 => (40, true), // ' / "
        0x60 => (41, false), 0x7e => (41, true), // ` / ~
        0x2c => (51, false), 0x3c => (51, true), // , / <
        0x2e => (52, false), 0x3e => (52, true), // . / >
        0x2f => (53, false), 0x3f => (53, true), // / / ?
        // Special keys
        0xff08 => (14, false),  // BackSpace
        0xff09 => (15, false),  // Tab
        0xff0d => (28, false),  // Return
        0xff1b => (1, false),   // Escape
        0xff63 => (110, false), // Insert
        0xffff => (111, false), // Delete
        0xff50 => (102, false), // Home
        0xff57 => (107, false), // End
        0xff55 => (104, false), // Page_Up
        0xff56 => (109, false), // Page_Down
        0xff51 => (105, false), // Left
        0xff52 => (103, false), // Up
        0xff53 => (106, false), // Right
        0xff54 => (108, false), // Down
        // Function keys
        0xffbe => (59, false), 0xffbf => (60, false), 0xffc0 => (61, false), 0xffc1 => (62, false),
        0xffc2 => (63, false), 0xffc3 => (64, false), 0xffc4 => (65, false), 0xffc5 => (66, false),
        0xffc6 => (67, false), 0xffc7 => (68, false), 0xffc8 => (87, false), 0xffc9 => (88, false),
        // Modifiers
        0xffe1 => (42, false),  // Shift_L
        0xffe2 => (54, false),  // Shift_R
        0xffe3 => (29, false),  // Control_L
        0xffe4 => (97, false),  // Control_R
        0xffe9 => (56, false),  // Alt_L
        0xffea => (100, false), // Alt_R
        0xffeb => (125, false), // Super_L
        _ => return None,
    };
    Some(pair)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_rect_finds_changed_region_and_skips_identical() {
        let a = vec![10u8; 4 * 4 * 4];
        assert_eq!(compute_rgba_diff_rect(&a, &a, 4, 4), None);

        let mut b = a.clone();
        // Mutate pixel at (2, 1) and (3, 2)
        b[(1 * 4 + 2) * 4] = 99;
        b[(2 * 4 + 3) * 4 + 1] = 88;
        let rect = compute_rgba_diff_rect(&a, &b, 4, 4).unwrap();
        assert_eq!(rect, (2, 1, 2, 2));
        let patch = extract_rgba_subrect(&b, 4, 2, 1, 2, 2);
        assert_eq!(patch.len(), 2 * 2 * 4);
        assert_eq!(patch[0], 99);
    }

    #[test]
    fn keysym_maps_to_evdev() {
        assert_eq!(keysym_to_evdev(b'a' as u32), Some((30, false)));
        assert_eq!(keysym_to_evdev(b'A' as u32), Some((30, true)));
        assert_eq!(keysym_to_evdev(0xff0d), Some((28, false)));
        assert_eq!(keysym_to_evdev(0xffe3), Some((29, false)));
    }
}
