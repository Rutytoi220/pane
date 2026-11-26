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
