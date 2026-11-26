//! Stdio transport layer: spawns either `ssh -T user@host <pane-host>` or a
//! local `pane-host` (`--demo`), and exchanges framed `ClientMsg` / `HostMsg`
//! packets on background threads.
//!
//! When connecting over SSH with the default `--host-bin` (`"pane-host"`), the
//! static `x86_64-unknown-linux-musl` `pane-host` binary embedded in this
//! executable is automatically deployed to `~/.cache/pane/pane-host-<version>`
//! on the remote machine if not already present.

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, unbounded};
use pane_proto::{ClientMsg, FrameError, HostMsg, read_frame, write_frame};

/// Default CLI `--host-bin` value that triggers zero-config SSH auto-deploy.
const DEFAULT_HOST_BIN: &str = "pane-host";

/// Static `x86_64-unknown-linux-musl` build of `pane-host` embedded for
/// zero-config deployment onto any x86_64 Linux host.
const EMBEDDED_HOST_BIN: &[u8] =
    include_bytes!("../../../target/x86_64-unknown-linux-musl/release/pane-host");

/// Cached remote path where the embedded agent is deployed.
const REMOTE_DEPLOY_PATH: &str = concat!("~/.cache/pane/pane-host-", env!("CARGO_PKG_VERSION"));

/// Atomic remote install command: skips upload if the versioned binary already
/// exists and is executable; otherwise streams stdin to a `.tmp` file, marks it
/// executable, and atomically renames it into place.
const REMOTE_BOOTSTRAP_CMD: &str = concat!(
    "mkdir -p ~/.cache/pane && if [ ! -x ~/.cache/pane/pane-host-",
    env!("CARGO_PKG_VERSION"),
    " ]; then cat > ~/.cache/pane/pane-host-",
    env!("CARGO_PKG_VERSION"),
    ".tmp && chmod +x ~/.cache/pane/pane-host-",
    env!("CARGO_PKG_VERSION"),
    ".tmp && mv ~/.cache/pane/pane-host-",
    env!("CARGO_PKG_VERSION"),
    ".tmp ~/.cache/pane/pane-host-",
    env!("CARGO_PKG_VERSION"),
    "; fi",
);

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
        // Zero-config SSH bootstrap when `--host-bin` is left at its default.
        let effective_remote_bin = match mode {
            LaunchMode::Ssh { target, host_bin, ssh_opts } if host_bin == DEFAULT_HOST_BIN => {
                bootstrap_remote_host(target, ssh_opts)?;
                Some(REMOTE_DEPLOY_PATH)
            }
            _ => None,
        };

        let mut cmd = build_command(mode, effective_remote_bin);
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

fn ssh_base_command(target: &str, ssh_opts: &[String]) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-T"); // Disable remote PTY so stdio stays pure binary.
    cmd.args(["-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"]);
    for opt in ssh_opts {
        cmd.args(["-o", opt]);
    }
    cmd.arg(target);
    cmd
}

/// Ensure the embedded static `pane-host` binary is installed at
/// [`REMOTE_DEPLOY_PATH`] on `target`.
fn bootstrap_remote_host(target: &str, ssh_opts: &[String]) -> Result<()> {
    let mut cmd = ssh_base_command(target, ssh_opts);
    cmd.arg(REMOTE_BOOTSTRAP_CMD)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn ssh while bootstrapping pane-host on {target}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        // If the remote already has the cached binary, the remote shell exits
        // without running `cat`, which can surface as BrokenPipe here.
        if let Err(e) = stdin.write_all(EMBEDDED_HOST_BIN)
            && e.kind() != io::ErrorKind::BrokenPipe
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e).with_context(|| format!("failed to stream pane-host binary to {target}"));
        }
        // Explicitly close stdin so remote `cat` sees EOF.
        drop(stdin);
    }

    let output = child
        .wait_with_output()
        .with_context(|| format!("failed while waiting for remote bootstrap on {target}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            bail!(
                "SSH bootstrap of pane-host on {target} failed (exit status: {}). \
                 Verify SSH authentication and remote shell access.",
                output.status
            );
        } else {
            bail!(
                "SSH bootstrap of pane-host on {target} failed (exit status: {}): {stderr}",
                output.status
            );
        }
    }

    Ok(())
}

fn build_command(mode: &LaunchMode, deployed_remote_bin: Option<&str>) -> Command {
    match mode {
        LaunchMode::Ssh { target, host_bin, ssh_opts } => {
            let mut cmd = ssh_base_command(target, ssh_opts);
            cmd.arg(deployed_remote_bin.unwrap_or(host_bin.as_str()));
            cmd
        }
        LaunchMode::Demo { host_bin_override } => {
            let bin = if let Some(custom) = host_bin_override {
                PathBuf::from(custom)
            } else {
                locate_local_host_bin()
            };
            Command::new(bin)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_host_binary_is_static_elf() {
        assert!(EMBEDDED_HOST_BIN.len() > 100_000, "embedded musl binary unexpectedly small");
        assert_eq!(&EMBEDDED_HOST_BIN[..4], b"\x7fELF", "embedded binary must be an ELF executable");
    }

    #[test]
    fn bootstrap_and_deploy_commands_match_expected() {
        assert_eq!(REMOTE_DEPLOY_PATH, "~/.cache/pane/pane-host-0.1.0");
        assert_eq!(
            REMOTE_BOOTSTRAP_CMD,
            "mkdir -p ~/.cache/pane && if [ ! -x ~/.cache/pane/pane-host-0.1.0 ]; then cat > ~/.cache/pane/pane-host-0.1.0.tmp && chmod +x ~/.cache/pane/pane-host-0.1.0.tmp && mv ~/.cache/pane/pane-host-0.1.0.tmp ~/.cache/pane/pane-host-0.1.0; fi"
        );
    }

    #[test]
    fn build_command_respects_custom_vs_deployed_host_bin() {
        let mode = LaunchMode::Ssh {
            target: "dev@example.com".into(),
            host_bin: "/opt/bin/pane-host".into(),
            ssh_opts: vec!["Port=2222".into()],
        };
        let custom = build_command(&mode, None);
        let args: Vec<_> = custom.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args.last().map(String::as_str), Some("/opt/bin/pane-host"));
        assert!(args.contains(&"Port=2222".to_string()));

        let deployed = build_command(&mode, Some(REMOTE_DEPLOY_PATH));
        let args2: Vec<_> = deployed.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args2.last().map(String::as_str), Some("~/.cache/pane/pane-host-0.1.0"));
    }
}
