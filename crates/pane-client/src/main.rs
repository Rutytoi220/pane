//! `pane` — native GPU-accelerated remote workspace over SSH stdio.

mod app;
mod files_view;
mod metrics;
mod monitor_view;
mod terminal_view;
mod theme;
mod transport;

use anyhow::{Context, Result, bail};
use clap::Parser;

/// Zero-daemon, retained-mode remote desktop workspace over SSH stdio.
#[derive(Debug, Parser)]
#[command(name = "pane", version, about)]
struct Cli {
    /// SSH destination (e.g. `user@host` or an `~/.ssh/config` host alias).
    /// Omit when using `--demo`.
    target: Option<String>,

    /// Path to `pane-host` on the remote machine.
    #[arg(long, default_value = "pane-host")]
    host_bin: String,

    /// Extra `-o` option passed through to `ssh` (repeatable).
    #[arg(long = "ssh-opt", value_name = "KEY=VAL")]
    ssh_opts: Vec<String>,

    /// Run a local `pane-host` child process over stdio pipes (100% offline demo).
    #[arg(long)]
    demo: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let mode = if cli.demo {
        transport::LaunchMode::Demo { host_bin_override: (cli.host_bin != "pane-host").then_some(cli.host_bin) }
    } else if let Some(target) = cli.target {
        transport::LaunchMode::Ssh { target, host_bin: cli.host_bin, ssh_opts: cli.ssh_opts }
    } else {
        bail!("provide an SSH target (e.g. `pane user@host`) or run `pane --demo`");
    };

    let title = format!("pane — {}", mode.label());
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
            Ok(Box::new(app::PaneApp::new(cc.egui_ctx.clone(), mode)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
    .context("failed to start native GPU workspace")
}
