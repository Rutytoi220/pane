//! Multiplexed PTY terminal manager with headless `vt100` emulation and
//! ~60 Hz coalesced state-diff streaming.
//!
//! Each terminal owns:
//! - a `portable-pty` master/slave pair running the user's interactive shell,
//! - a reader thread that drains PTY output into a `vt100::Parser` behind a
//!   mutex, marking the terminal dirty,
//! - a snapshot of the last grid sent to the client so the 60 Hz ticker emits
//!   only rows whose text or SGR attributes actually changed.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use pane_proto::{CellStyle, Color, HostMsg, Row, RowBuilder, StyleFlags, diff_rows};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::outbox::Outbox;

/// Coalesced diff cadence (~60 Hz).
const TICK_INTERVAL: Duration = Duration::from_millis(16);
/// Scrollback buffer retained per terminal on the host.
const SCROLLBACK_LINES: usize = 2000;

struct TitleTracker {
    pending_title: Option<String>,
}

impl vt100::Callbacks for TitleTracker {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        if let Ok(s) = std::str::from_utf8(title) {
            self.pending_title = Some(s.to_owned());
        }
    }
}

struct TermState {
    parser: vt100::Parser<TitleTracker>,
    dirty: bool,
    last_rows: Vec<Row>,
    last_cursor: (u16, u16, bool),
    last_modes: (bool, bool, u32),
    last_size: (u16, u16),
    exited: Option<Option<u32>>,
}

struct TermHandle {
    state: Arc<Mutex<TermState>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

enum Cmd {
    Open { term_id: u32, cols: u16, rows: u16 },
    Input { term_id: u32, data: Vec<u8> },
    Resize { term_id: u32, cols: u16, rows: u16 },
    Scroll { term_id: u32, delta: i32 },
    Close { term_id: u32 },
    Shutdown,
}

pub struct TerminalManager {
    tx: Sender<Cmd>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TerminalManager {
    pub fn spawn(outbox: Outbox) -> Self {
        let (tx, rx) = unbounded();
        let handle = thread::Builder::new()
            .name("term-manager".into())
            .spawn(move || manager_loop(rx, outbox))
            .expect("spawn terminal manager");
        Self { tx, handle: Some(handle) }
    }

    pub fn open(&self, term_id: u32, cols: u16, rows: u16) {
        let _ = self.tx.send(Cmd::Open { term_id, cols, rows });
    }

    pub fn input(&self, term_id: u32, data: &[u8]) {
        let _ = self.tx.send(Cmd::Input { term_id, data: data.to_vec() });
    }

    pub fn resize(&self, term_id: u32, cols: u16, rows: u16) {
        let _ = self.tx.send(Cmd::Resize { term_id, cols, rows });
    }

    pub fn scroll(&self, term_id: u32, delta: i32) {
        let _ = self.tx.send(Cmd::Scroll { term_id, delta });
    }

    pub fn close(&self, term_id: u32) {
        let _ = self.tx.send(Cmd::Close { term_id });
    }

    pub fn shutdown(mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn manager_loop(rx: Receiver<Cmd>, outbox: Outbox) {
    let mut terms: HashMap<u32, TermHandle> = HashMap::new();
    let tick = crossbeam_channel::tick(TICK_INTERVAL);

    loop {
        crossbeam_channel::select! {
            recv(rx) -> cmd => match cmd {
                Ok(Cmd::Open { term_id, cols, rows }) => {
                    if let Some(old) = terms.remove(&term_id) {
                        kill_term(old);
                    }
                    match spawn_term(term_id, cols, rows) {
                        Ok(handle) => {
                            log!("opened terminal {term_id} ({cols}x{rows})");
                            terms.insert(term_id, handle);
                        }
                        Err(e) => {
                            log!("failed to open terminal {term_id}: {e}");
                            outbox.send(HostMsg::Error {
                                req_id: None,
                                message: format!("open terminal {term_id}: {e}"),
                            });
                            outbox.send(HostMsg::TermClosed { term_id, exit_code: None });
                        }
                    }
                }
                Ok(Cmd::Input { term_id, data }) => {
                    if let Some(term) = terms.get_mut(&term_id) {
                        // Typing jumps back to the live viewport if scrolled up.
                        if let Ok(mut st) = term.state.lock()
                            && st.parser.screen().scrollback() > 0
                        {
                            st.parser.screen_mut().set_scrollback(0);
                            st.dirty = true;
                        }
                        if let Err(e) = term.writer.write_all(&data).and_then(|()| term.writer.flush()) {
                            log!("pty write failed on terminal {term_id}: {e}");
                        }
                    }
                }
                Ok(Cmd::Resize { term_id, cols, rows }) => {
                    if let Some(term) = terms.get_mut(&term_id) {
                        let cols = cols.max(2);
                        let rows = rows.max(1);
                        let _ = term.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                        if let Ok(mut st) = term.state.lock() {
                            let nz_rows = NonZeroU16::new(rows).unwrap();
                            let nz_cols = NonZeroU16::new(cols).unwrap();
                            st.parser.set_size(nz_rows, nz_cols);
                            st.dirty = true;
                        }
                    }
                }
                Ok(Cmd::Scroll { term_id, delta }) => {
                    if let Some(term) = terms.get(&term_id)
                        && let Ok(mut st) = term.state.lock()
                    {
                        let cur = st.parser.screen().scrollback();
                        let max_sb = st.parser.screen().scrollback_len();
                        let next = if delta >= 0 {
                            cur.saturating_add(delta as usize).min(max_sb)
                        } else {
                            cur.saturating_sub(delta.unsigned_abs() as usize)
                        };
                        if next != cur {
                            st.parser.screen_mut().set_scrollback(next);
                            st.dirty = true;
                        }
                    }
                }
                Ok(Cmd::Close { term_id }) => {
                    if let Some(term) = terms.remove(&term_id) {
                        log!("closing terminal {term_id}");
                        kill_term(term);
                    }
                }
                Ok(Cmd::Shutdown) | Err(_) => break,
            },
            recv(tick) -> _ => {
                flush_diffs(&mut terms, &outbox);
            }
        }
    }

    for (_, term) in terms.drain() {
        kill_term(term);
    }
}

fn kill_term(mut term: TermHandle) {
    let _ = term.killer.kill();
}

fn spawn_term(term_id: u32, cols: u16, rows: u16) -> Result<TermHandle, String> {
    let cols = cols.max(2);
    let rows = rows.max(1);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| format!("openpty: {e}"))?;

    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    let mut cmd = CommandBuilder::new(&shell);
    cmd.cwd(crate::files::home_dir());
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("PANE_SESSION", "1");

    let mut child = pair.slave.spawn_command(cmd).map_err(|e| format!("spawn {shell}: {e}"))?;
    let killer = child.clone_killer();
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().map_err(|e| format!("clone pty reader: {e}"))?;
    let writer = pair.master.take_writer().map_err(|e| format!("take pty writer: {e}"))?;

    let nz_rows = NonZeroU16::new(rows).unwrap();
    let nz_cols = NonZeroU16::new(cols).unwrap();
    let parser = vt100::Parser::new_with_callbacks(
        nz_rows,
        nz_cols,
        SCROLLBACK_LINES,
        TitleTracker { pending_title: None },
    );

    let state = Arc::new(Mutex::new(TermState {
        parser,
        dirty: true, // Send initial grid state immediately on first tick.
        last_rows: Vec::new(),
        last_cursor: (0, 0, true),
        last_modes: (false, false, 0),
        last_size: (0, 0),
        exited: None,
    }));

    let state_reader = Arc::clone(&state);
    thread::Builder::new()
        .name(format!("pty-read-{term_id}"))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut st) = state_reader.lock() {
                            st.parser.process(&buf[..n]);
                            st.dirty = true;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let exit_code = child.wait().ok().map(|s| s.exit_code());
            if let Ok(mut st) = state_reader.lock() {
                st.exited = Some(exit_code);
                st.dirty = true;
            }
        })
        .map_err(|e| format!("spawn pty reader: {e}"))?;

    Ok(TermHandle { state, master: pair.master, writer, killer })
}

fn flush_diffs(terms: &mut HashMap<u32, TermHandle>, outbox: &Outbox) {
    let mut closed: Vec<(u32, Option<u32>)> = Vec::new();

    for (&term_id, handle) in terms.iter() {
        let Ok(mut st) = handle.state.lock() else { continue };
        if !st.dirty {
            continue;
        }
        st.dirty = false;

        if let Some(title) = st.parser.callbacks_mut().pending_title.take() {
            outbox.send(HostMsg::TermTitle { term_id, title });
        }

        let (rows, cols, cursor, modes, snapshot) = extract_screen(st.parser.screen());
        let dirty_rows = diff_rows(&st.last_rows, &snapshot);
        let meta_changed =
            st.last_cursor != cursor || st.last_modes != modes || st.last_size != (cols, rows);

        if !dirty_rows.is_empty() || meta_changed {
            st.last_rows = snapshot;
            st.last_cursor = cursor;
            st.last_modes = modes;
            st.last_size = (cols, rows);
            outbox.send(HostMsg::TermDiff {
                term_id,
                screen_cols: cols,
                screen_rows: rows,
                cursor_row: cursor.0,
                cursor_col: cursor.1,
                cursor_visible: cursor.2,
                app_cursor: modes.0,
                bracketed_paste: modes.1,
                scrollback: modes.2,
                rows: dirty_rows,
            });
        }

        if let Some(exit_code) = st.exited.take() {
            closed.push((term_id, exit_code));
        }
    }

    for (term_id, exit_code) in closed {
        log!("terminal {term_id} exited (code: {exit_code:?})");
        terms.remove(&term_id);
        outbox.send(HostMsg::TermClosed { term_id, exit_code });
    }
}

fn extract_screen(
    screen: &vt100::Screen,
) -> (u16, u16, (u16, u16, bool), (bool, bool, u32), Vec<Row>) {
    let (nz_rows, nz_cols) = screen.size();
    let rows = nz_rows.get();
    let cols = nz_cols.get();

    let mut out = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let mut builder = RowBuilder::new();
        for c in 0..cols {
            let Some(cell) = screen.cell(r, c) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let width = if cell.is_wide() { 2 } else { 1 };
            let text = if cell.has_contents() { cell.contents() } else { " " };
            builder.push_cell(text, width, convert_style(cell));
        }
        out.push(builder.finish());
    }

    let (cur_row, cur_col) = screen.cursor_position();
    let cursor = (cur_row, cur_col, !screen.hide_cursor());
    let modes = (screen.application_cursor(), screen.bracketed_paste(), screen.scrollback() as u32);
    (rows, cols, cursor, modes, out)
}

fn convert_style(cell: &vt100::Cell) -> CellStyle {
    let mut flags = StyleFlags::NONE;
    if cell.bold() {
        flags |= StyleFlags::BOLD;
    }
    if cell.italic() {
        flags |= StyleFlags::ITALIC;
    }
    if cell.underline() {
        flags |= StyleFlags::UNDERLINE;
    }
    if cell.inverse() {
        flags |= StyleFlags::INVERSE;
    }
    if cell.dim() {
        flags |= StyleFlags::DIM;
    }
    CellStyle { fg: convert_color(cell.fgcolor()), bg: convert_color(cell.bgcolor()), flags }
}

fn convert_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Default,
        vt100::Color::Idx(i) => Color::Idx(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vt100_screen_extracts_styled_rows_and_diffs_minimally() {
        let mut parser = vt100::Parser::new(
            NonZeroU16::new(4).unwrap(),
            NonZeroU16::new(20).unwrap(),
            0,
        );
        parser.process(b"\x1b[1;32mhello\x1b[0m world\r\nline two");
        let (rows, cols, cursor, _, snap1) = extract_screen(parser.screen());
        assert_eq!((rows, cols), (4, 20));
        assert_eq!((cursor.0, cursor.1), (1, 8));
        assert_eq!(snap1[0].len(), 2);
        assert_eq!(snap1[0][0].text, "hello");
        assert_eq!(snap1[0][0].style.fg, Color::Idx(2));
        assert!(snap1[0][0].style.flags.contains(StyleFlags::BOLD));
        assert_eq!(snap1[0][1].text, " world");
        assert_eq!(snap1[1][0].text, "line two");
        assert!(snap1[2].is_empty());

        // Update only row 1: diff should contain exactly row 1.
        parser.process(b"!");
        let (_, _, _, _, snap2) = extract_screen(parser.screen());
        let d = diff_rows(&snap1, &snap2);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].row, 1);
        assert_eq!(d[0].spans[0].text, "line two!");
    }
}
