//! pane-host — the headless remote agent.
//!
//! Launched by the client as `ssh -T user@host pane-host` (or locally via
//! `pane --demo`). It speaks the pane binary protocol over stdin/stdout:
//!
//! * **stdout is sacred**: only binary `HostMsg` frames are ever written to it.
//! * every diagnostic goes to **stderr** via the [`log!`] macro.
//!
//! There is no daemon: the process lives exactly as long as the SSH session.

/// Log to stderr. Never use `println!` in this crate: stdout carries frames.
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("[pane-host] {}", format_args!($($arg)*))
    };
}

mod files;
mod outbox;
mod sysinfo;
mod telemetry;
mod terminal;

use std::io::{self, BufReader};

use pane_proto::{ClientMsg, FrameError, HostMsg, PROTOCOL_VERSION, read_frame};

use crate::outbox::Outbox;
use crate::terminal::TerminalManager;

fn main() {
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                eprintln!("pane-host {} (protocol v{PROTOCOL_VERSION})", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                eprintln!(
                    "pane-host {}\n\nHeadless pane agent. Speaks the pane binary protocol on stdin/stdout.\n\
                     It is normally launched by the `pane` client, e.g. `ssh -T user@host pane-host`.",
                    env!("CARGO_PKG_VERSION")
                );
                return;
            }
            other => log!("ignoring unknown argument {other:?}"),
        }
    }

    log!("starting (pid {}, protocol v{PROTOCOL_VERSION})", std::process::id());

    let outbox = Outbox::spawn(io::stdout());
    let terminals = TerminalManager::spawn(outbox.clone());
    let mut telemetry_started = false;

    let mut stdin = BufReader::with_capacity(64 * 1024, io::stdin().lock());
    loop {
        let msg = match read_frame::<_, ClientMsg>(&mut stdin) {
            Ok(Some((msg, _))) => msg,
            Ok(None) => {
                log!("stdin closed, shutting down");
                break;
            }
            Err(FrameError::Io(e)) => {
                log!("stdin error: {e}");
                break;
            }
            Err(FrameError::Truncated) => {
                log!("stdin closed mid-frame, shutting down");
                break;
            }
            Err(FrameError::TooLarge(n)) => {
                log!("fatal: incoming frame claims {n} bytes; stream is corrupt");
                break;
            }
            Err(e) => {
                // The frame was fully consumed; skip it and keep the session alive.
                log!("dropping undecodable client frame: {e}");
                continue;
            }
        };

        match msg {
            ClientMsg::Hello { protocol_version } => {
                if protocol_version != PROTOCOL_VERSION {
                    log!("client speaks protocol v{protocol_version}, host speaks v{PROTOCOL_VERSION}");
                    outbox.send(HostMsg::Error {
                        req_id: None,
                        message: format!(
                            "protocol mismatch: client v{protocol_version}, host v{PROTOCOL_VERSION}; \
                             please update pane-host on the remote machine"
                        ),
                    });
                }
                let info = sysinfo::collect();
                log!("handshake: {} ({}), {} cores", info.hostname, info.os_info, info.cpu_cores);
                outbox.send(HostMsg::Welcome {
                    protocol_version: PROTOCOL_VERSION,
                    hostname: info.hostname,
                    os_info: info.os_info,
                    cpu_cores: info.cpu_cores,
                    total_mem_mb: info.total_mem_mb,
                    home_dir: files::home_dir().to_string_lossy().into_owned(),
                });
                if !telemetry_started {
                    telemetry::spawn(outbox.clone());
                    telemetry_started = true;
                }
            }
            ClientMsg::Ping { timestamp_micros } => outbox.send(HostMsg::Pong { timestamp_micros }),
            ClientMsg::OpenTerminal { term_id, cols, rows } => terminals.open(term_id, cols, rows),
            ClientMsg::TermInput { term_id, data } => terminals.input(term_id, &data),
            ClientMsg::TermResize { term_id, cols, rows } => terminals.resize(term_id, cols, rows),
            ClientMsg::TermScroll { term_id, delta } => terminals.scroll(term_id, delta),
            ClientMsg::CloseTerminal { term_id } => terminals.close(term_id),
            ClientMsg::ListDir { req_id, path } => {
                let reply = match files::list_dir(&path) {
                    Ok((path, entries)) => HostMsg::DirListing { req_id, path, entries },
                    Err(message) => HostMsg::Error { req_id: Some(req_id), message },
                };
                outbox.send(reply);
            }
            ClientMsg::DownloadFile { req_id, path } => files::spawn_download(req_id, path, outbox.clone()),
        }
    }

    terminals.shutdown();
    outbox.shutdown();
}
