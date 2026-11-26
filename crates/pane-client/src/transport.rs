//! Stdio transport layer: spawns either `ssh -T user@host pane-host` or a
//! local `pane-host` (`--demo`), and exchanges framed `ClientMsg` / `HostMsg`
//! packets on background threads.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, unbounded};
use pane_proto::{ClientMsg, FrameError, HostMsg, read_frame, write_frame};

/// How the client connects to `pane-host`.
#[derive(Debug, Clone)]
pub enum LaunchMode {
    Ssh { target: String, host_bin: String, ssh_opts: Vec<String> },
    Demo { host_bin_override: Option<String> },
}

impl LaunchMode {
    pub fn label(&self) -> String {
        match self {
            Self::Ssh { target, .. } => target.clone(),
            Self::Demo { .. } => "local demo".into(),
        }
    }

    pub fn is_demo(&self) -> bool {
        matches!(self, Self::Demo { .. })
    }
}

/// Events emitted by the background transport threads toward the UI.
pub enum TransportEvent {
    Msg { msg: HostMsg, wire_bytes: usize },
    Sent { wire_bytes: usize },
    Stderr(String),
    Disconnected(String),
}

pub struct Transport {
    tx: Sender<ClientMsg>,
    pub rx: Receiver<TransportEvent>,
    child: Option<Child>,
}

impl Transport {
    pub fn connect(mode: &LaunchMode, ctx: egui::Context) -> Result<Self> {
        let mut cmd = build_command(mode)?;
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| match mode {
            LaunchMode::Ssh { target, .. } => format!("failed to spawn ssh to {target}"),
            LaunchMode::Demo { .. } => "failed to spawn local pane-host (build it with `cargo build --workspace`)".into(),
        })?;

        let stdin = child.stdin.take().context("child stdin missing")?;
        let stdout = child.stdout.take().context("child stdout missing")?;
        let stderr = child.stderr.take().context("child stderr missing")?;

        let (cmd_tx, cmd_rx) = unbounded::<ClientMsg>();
        let (ev_tx, ev_rx) = unbounded::<TransportEvent>();

        // Writer thread: encodes ClientMsg frames to child stdin.
        let ev_tx_w = ev_tx.clone();
        let ctx_w = ctx.clone();
        thread::Builder::new()
            .name("pane-tx".into())
            .spawn(move || {
                let mut w = BufWriter::with_capacity(64 * 1024, stdin);
                while let Ok(msg) = cmd_rx.recv() {
                    match write_frame(&mut w, &msg).and_then(|n| w.flush().map(|()| n).map_err(FrameError::Io)) {
                        Ok(wire_bytes) => {
                            let _ = ev_tx_w.send(TransportEvent::Sent { wire_bytes });
                            ctx_w.request_repaint();
                        }
                        Err(e) => {
                            let _ = ev_tx_w.send(TransportEvent::Disconnected(format!("write failed: {e}")));
                            ctx_w.request_repaint();
                            break;
                        }
                    }
                }
            })
            .context("spawn writer thread")?;

        // Reader thread: decodes HostMsg frames from child stdout.
        let ev_tx_r = ev_tx.clone();
        let ctx_r = ctx.clone();
        thread::Builder::new()
            .name("pane-rx".into())
            .spawn(move || {
                let mut r = BufReader::with_capacity(128 * 1024, stdout);
                loop {
                    match read_frame::<_, HostMsg>(&mut r) {
                        Ok(Some((msg, wire_bytes))) => {
                            if ev_tx_r.send(TransportEvent::Msg { msg, wire_bytes }).is_err() {
                                break;
                            }
                            ctx_r.request_repaint();
                        }
                        Ok(None) => {
                            let _ = ev_tx_r.send(TransportEvent::Disconnected("remote host closed stream".into()));
                            ctx_r.request_repaint();
                            break;
                        }
                        Err(e) => {
                            let _ = ev_tx_r.send(TransportEvent::Disconnected(format!("stream error: {e}")));
                            ctx_r.request_repaint();
                            break;
                        }
                    }
                }
            })
            .context("spawn reader thread")?;

        // Stderr thread: forwards host diagnostics to local stderr + UI log.
        thread::Builder::new()
            .name("pane-stderr".into())
            .spawn(move || {
                let r = BufReader::new(stderr);
                for line in r.lines().map_while(Result::ok) {
                    eprintln!("[remote] {line}");
                    if ev_tx.send(TransportEvent::Stderr(line)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .context("spawn stderr thread")?;

        Ok(Self { tx: cmd_tx, rx: ev_rx, child: Some(child) })
    }

    pub fn send(&self, msg: ClientMsg) {
        let _ = self.tx.send(msg);
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn build_command(mode: &LaunchMode) -> Result<Command> {
    match mode {
        LaunchMode::Ssh { target, host_bin, ssh_opts } => {
            let mut cmd = Command::new("ssh");
            cmd.arg("-T"); // Disable remote PTY so stdout stays pure binary.
            cmd.args(["-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"]);
            for opt in ssh_opts {
                cmd.args(["-o", opt]);
            }
            cmd.arg(target);
            cmd.arg(host_bin);
            Ok(cmd)
        }
        LaunchMode::Demo { host_bin_override } => {
            let bin = if let Some(custom) = host_bin_override {
                PathBuf::from(custom)
            } else {
                locate_local_host_bin()
            };
            Ok(Command::new(bin))
        }
    }
}

/// Prefer a sibling `pane-host` next to the running `pane` binary (works out
/// of the box with `cargo run --bin pane -- --demo`), falling back to `PATH`.
fn locate_local_host_bin() -> PathBuf {
    let exe_name = if cfg!(windows) { "pane-host.exe" } else { "pane-host" };
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(exe_name);
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from(exe_name)
}
