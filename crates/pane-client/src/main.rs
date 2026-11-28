//! `pane` — native GPU-accelerated remote workspace over SSH stdio.

mod app;
mod files_view;
mod gui_window_view;
mod launcher;
mod metrics;
mod monitor_view;
mod processes_view;
mod services_view;
mod terminal_view;
mod theme;
mod transport;
mod viewer_view;

use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use clap::Parser;

const DEFAULT_TARGET: &str = "rutytoi@100.100.33.5";
const DETACH_ENV: &str = "PANE_DETACHED";

/// Zero-daemon, retained-mode remote desktop workspace over SSH stdio.
#[derive(Debug, Parser)]
#[command(name = "pane", version, about)]
struct Args {
    /// SSH destination (e.g. `user@host` or an `~/.ssh/config` host alias).
    /// Defaults to `rutytoi@100.100.33.5` unless `--demo` is passed.
    #[arg(default_value = DEFAULT_TARGET)]
    target: String,

    /// Path to `pane-host` on the remote machine.
    #[arg(long, default_value = "pane-host")]
    host_bin: String,

    /// Extra `-o` option passed through to `ssh` (repeatable).
    #[arg(long = "ssh-opt", value_name = "KEY=VAL")]
    ssh_opts: Vec<String>,

    /// Run a local `pane-host` child process over stdio pipes (100% offline demo).
    #[arg(long)]
    demo: bool,

    /// Stay attached to the launching terminal and stream logs to stderr.
    #[arg(long, short = 'f')]
    foreground: bool,

    /// Terminal-only mode (no live desktop stream).
    #[arg(long)]
    shell: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if !args.foreground && std::env::var_os(DETACH_ENV).is_none() {
        spawn_detached()?;
        return Ok(());
    }

    let mode = if args.demo {
        transport::LaunchMode::Demo { host_bin_override: (args.host_bin != "pane-host").then_some(args.host_bin) }
    } else {
        transport::LaunchMode::Ssh { target: args.target, host_bin: args.host_bin, ssh_opts: args.ssh_opts }
    };

    let title = format!("pane - {}", mode.label());
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(&title)
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "pane",
        native_options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            Ok(Box::new(app::PaneApp::new(cc.egui_ctx.clone(), mode, !args.shell)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
    .context("failed to start native GPU workspace")
}

/// Re-exec `pane` in a new session (`setsid`) with stdio detached from the
/// launching terminal so the shell prompt returns immediately and doesn't stay
/// open with remote logs. Logs go to `~/.cache/pane/pane.log`.
fn spawn_detached() -> Result<()> {
    let exe = std::env::current_exe().context("locate current pane executable")?;
    let log_dir = dirs::cache_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("pane");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path: PathBuf = log_dir.join("pane.log");
    let stderr_target = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());

    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1))
        .env(DETACH_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_target);
    // SAFETY: `setsid` is async-signal-safe and detaches the GUI child from the terminal session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("detach pane GUI process")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_defaults_to_default_target() {
        let a = Args::try_parse_from(["pane"]).unwrap();
        assert!(!a.demo);
        assert!(!a.foreground);
        assert_eq!(a.target, DEFAULT_TARGET);
        assert_eq!(a.host_bin, "pane-host");
    }

    #[test]
    fn cli_allows_demo_foreground_and_custom_target_override() {
        let demo = Args::try_parse_from(["pane", "--demo", "-f"]).unwrap();
        assert!(demo.demo);
        assert!(demo.foreground);

        let custom = Args::try_parse_from(["pane", "otheruser@otherhost", "--host-bin", "/opt/pane-host"]).unwrap();
        assert!(!custom.demo);
        assert_eq!(custom.target, "otheruser@otherhost");
        assert_eq!(custom.host_bin, "/opt/pane-host");
    }
}
