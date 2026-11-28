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

/// Compute a fast deterministic 64-bit FNV-1a fingerprint of the embedded binary
/// so any rebuild of `pane-host` automatically invalidates stale remote caches.
fn embedded_bin_hash() -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in EMBEDDED_HOST_BIN {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Content-addressed cached remote path (`$HOME/.cache/pane/pane-host-<ver>-<hash>`).
fn remote_deploy_path() -> String {
    format!("$HOME/.cache/pane/pane-host-{}-{:016x}", env!("CARGO_PKG_VERSION"), embedded_bin_hash())
}

/// Atomic remote install command: skips upload if this exact binary fingerprint
/// is already installed; otherwise writes to `.tmp`, marks executable, swaps in
/// atomically, and prunes older cached `pane-host-*` binaries.
fn remote_bootstrap_cmd(deploy_path: &str) -> String {
    format!(
        "mkdir -p \"$HOME/.cache/pane\" && if [ ! -x \"{deploy_path}\" ]; then \
         cat > \"{deploy_path}.tmp\" && chmod +x \"{deploy_path}.tmp\" && \
         mv \"{deploy_path}.tmp\" \"{deploy_path}\" && \
         find \"$HOME/.cache/pane\" -maxdepth 1 -type f -name 'pane-host-*' ! -path \"{deploy_path}\" -delete 2>/dev/null || true; \
         fi"
    )
}

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
                let deploy_path = remote_deploy_path();
                bootstrap_remote_host(target, ssh_opts, &deploy_path)?;
                Some(deploy_path)
            }
            _ => None,
        };

        let mut cmd = build_command(mode, effective_remote_bin.as_deref());
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

/// Ensure the embedded static `pane-host` binary is installed at `deploy_path`
/// on `target`.
fn bootstrap_remote_host(target: &str, ssh_opts: &[String], deploy_path: &str) -> Result<()> {
    let mut cmd = ssh_base_command(target, ssh_opts);
    cmd.arg(remote_bootstrap_cmd(deploy_path))
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
            // Inject session-env for ScreenCast compatibility (Desktop Portal needs
            // XDG_SESSION_TYPE=wayland/x11 rather than tty which blocks granting).
            // $DBUS_SESSION_BUS_ADDRESS and other creds are inherited from the SSH
            // PAM session; we force WAYLAND_DISPLAY to ensure portal matches the user
            // desktop where the consent dialog is shown.
            let remote_bin = deployed_remote_bin.unwrap_or(host_bin.as_str());
            cmd.args([
                "env",
                "XDG_SESSION_TYPE=wayland",
                "WAYLAND_DISPLAY=wayland-1",
                &remote_bin,
            ]);
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
        let deploy_path = remote_deploy_path();
        assert!(deploy_path.starts_with("$HOME/.cache/pane/pane-host-0.1.0-"));
        assert_eq!(deploy_path.len(), "$HOME/.cache/pane/pane-host-0.1.0-".len() + 16);
        let cmd = remote_bootstrap_cmd(&deploy_path);
        assert!(cmd.contains(&deploy_path));
        assert!(cmd.contains("chmod +x"));
        assert!(cmd.contains("-delete"));
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

        let deploy_path = remote_deploy_path();
        let deployed = build_command(&mode, Some(&deploy_path));
        let args2: Vec<_> = deployed.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args2.last().map(String::as_str), Some(deploy_path.as_str()));
    }
}
