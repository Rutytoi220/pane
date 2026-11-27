//! Strongly typed protocol messages.
//!
//! `ClientMsg` flows client → host (over the child's / ssh's stdin),
//! `HostMsg` flows host → client (over stdout). Both are serialized with
//! `postcard` and wrapped in a [`crate::frame`] envelope.

use serde::{Deserialize, Serialize};

use crate::diff::DirtyRow;

/// Bumped whenever the wire format changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Messages sent from the local GPU client to the remote headless host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// First message on the wire. The host replies with [`HostMsg::Welcome`].
    Hello { protocol_version: u32 },
    /// Latency probe; echoed back verbatim as [`HostMsg::Pong`].
    Ping { timestamp_micros: u64 },

    /// Spawn a new interactive shell on a fresh PTY.
    OpenTerminal { term_id: u32, cols: u16, rows: u16 },
    /// Raw keyboard / paste bytes destined for the PTY.
    TermInput { term_id: u32, data: Vec<u8> },
    /// The client-side viewport changed size.
    TermResize { term_id: u32, cols: u16, rows: u16 },
    /// Scroll the host-side scrollback viewport (positive = further back in history).
    TermScroll { term_id: u32, delta: i32 },
    /// Kill the shell and release the PTY.
    CloseTerminal { term_id: u32 },

    /// List a remote directory. An empty path or `~` means the host user's home.
    ListDir { req_id: u64, path: String },
    /// Stream a remote file back in [`crate::FILE_CHUNK_SIZE`] chunks.
    DownloadFile { req_id: u64, path: String },
    /// Read up to [`crate::MAX_TEXT_PREVIEW_BYTES`] of a remote UTF-8 text file for preview.
    ReadTextFile { req_id: u64, path: String },
    /// Save edited UTF-8 text back to a remote file.
    WriteTextFile { req_id: u64, path: String, content: String },

    /// Request a snapshot of top running processes on the remote host.
    ListProcesses { req_id: u64 },
    /// Send a POSIX signal (e.g. `15` = SIGTERM, `9` = SIGKILL) to a remote PID.
    KillProcess { req_id: u64, pid: u32, signal: i32 },

    /// Query running systemd services and Docker containers on the remote host.
    ListServices { req_id: u64 },
    /// Restart, stop, or start a systemd service unit.
    ControlService { req_id: u64, name: String, action: ServiceAction },
    /// Restart, stop, or start a Docker container by ID or name.
    ControlContainer { req_id: u64, id: String, action: ServiceAction },

    /// Launch a remote X11 GUI application inside the headless Xvfb bridge.
    LaunchGuiApp { req_id: u64, command: String },
    /// Move pointer relative to a streamed GUI window's top-left corner.
    GuiMouseMove { win_id: u32, x: i16, y: i16 },
    /// Press or release a mouse button (`1`=left, `2`=middle, `3`=right).
    GuiMouseButton { win_id: u32, button: u8, pressed: bool, x: i16, y: i16 },
    /// Scroll wheel event (`delta_y > 0` = scroll up / button 4, `< 0` = down / button 5).
    GuiScroll { win_id: u32, delta_y: i16, x: i16, y: i16 },
    /// Key press or release identified by X11 keysym.
    GuiKey { win_id: u32, keysym: u32, pressed: bool },
    /// Request closing a streamed GUI window (`WM_DELETE_WINDOW` / destroy).
    CloseGuiWindow { win_id: u32 },
    /// Pause or resume XDamage capture (e.g. when switching virtual workspaces).
    SetGuiWindowActive { win_id: u32, active: bool },
    /// Attach to the host's live X11/Xwayland root display (`:0`/`:1`, or fallback `Xvfb :99`).
    AttachDesktop { req_id: u64 },
    /// Notify the host of the client's desktop viewport size hint.
    ResizeDesktopView { win_id: u32, width: u16, height: u16 },
}

/// Messages sent from the remote headless host to the local GPU client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HostMsg {
    /// Handshake reply describing the remote machine.
    Welcome {
        protocol_version: u32,
        hostname: String,
        os_info: String,
        cpu_cores: u32,
        total_mem_mb: u64,
        home_dir: String,
    },
    /// Echo of [`ClientMsg::Ping`].
    Pong { timestamp_micros: u64 },

    /// Retained-mode terminal update: only rows whose text or attributes
    /// changed since the previous tick are included.
    TermDiff {
        term_id: u32,
        /// Current grid width in cells.
        screen_cols: u16,
        /// Current grid height in rows. Rows `>= screen_rows` must be dropped by the client.
        screen_rows: u16,
        cursor_row: u16,
        cursor_col: u16,
        cursor_visible: bool,
        /// DECCKM: arrow keys should be sent as `ESC O x` instead of `ESC [ x`.
        app_cursor: bool,
        /// Bracketed paste mode requested by the remote application.
        bracketed_paste: bool,
        /// How many rows the viewport is scrolled back into history (0 = live).
        scrollback: u32,
        rows: Vec<DirtyRow>,
    },
    /// The terminal's window title changed (OSC 0/2).
    TermTitle { term_id: u32, title: String },
    /// The shell exited or the PTY was closed.
    TermClosed { term_id: u32, exit_code: Option<u32> },

    /// Reply to [`ClientMsg::ListDir`]. `path` is the canonical absolute path.
    DirListing { req_id: u64, path: String, entries: Vec<DirEntry> },
    /// One chunk of a file requested with [`ClientMsg::DownloadFile`].
    FileChunk { req_id: u64, offset: u64, total_size: u64, data: Vec<u8>, eof: bool },

    /// Periodic (1 Hz) system telemetry.
    TelemetryUpdate {
        cpu_percent: f32,
        mem_used_mb: u64,
        load_avg: [f32; 3],
        /// Per-core utilisation, 0..=100.
        per_core: Vec<f32>,
        uptime_secs: u64,
    },

    /// A request failed. `req_id` refers to the originating request, if any.
    Error { req_id: Option<u64>, message: String },

    /// Reply to [`ClientMsg::ReadTextFile`].
    TextFileContent { req_id: u64, path: String, content: String, truncated: bool },
    /// Reply to [`ClientMsg::WriteTextFile`].
    WriteFileResult { req_id: u64, path: String, ok: bool, error: Option<String> },
    /// Reply to [`ClientMsg::ListProcesses`].
    ProcessList { req_id: u64, processes: Vec<ProcessInfo> },
    /// Reply to [`ClientMsg::ListServices`].
    ServiceSnapshot {
        req_id: u64,
        services: Vec<ServiceInfo>,
        containers: Vec<ContainerInfo>,
        systemd_available: bool,
        docker_available: bool,
    },
    /// Reply to [`ClientMsg::ControlService`] or [`ClientMsg::ControlContainer`].
    ControlResult { req_id: u64, target: String, ok: bool, message: String },

    /// A top-level GUI window was mapped on the host's headless Xvfb server.
    GuiWindowOpened { win_id: u32, title: String, width: u16, height: u16 },
    /// A streamed GUI window changed dimensions.
    GuiWindowResized { win_id: u32, width: u16, height: u16 },
    /// LZ4-compressed RGBA pixel patch for a damaged sub-rectangle of a GUI window.
    GuiWindowDamage { win_id: u32, x: u16, y: u16, width: u16, height: u16, rgba_lz4: Vec<u8> },
    /// A streamed GUI window was unmapped or destroyed.
    GuiWindowClosed { win_id: u32 },
}

/// Summary of one remote process from `/proc`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub rss_mb: f32,
    pub state: String,
}

/// Action to perform on a remote systemd service or Docker container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceAction {
    Start,
    Restart,
    Stop,
}

impl ServiceAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Restart => "restart",
            Self::Stop => "stop",
        }
    }
}

/// Summary of one systemd service unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub name: String,
    pub active_state: String,
    pub sub_state: String,
    pub description: String,
}

/// Summary of one Docker container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub state: String,
}

/// One entry of a remote directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    /// Seconds since the Unix epoch, when available.
    pub modified_unix: Option<i64>,
}
