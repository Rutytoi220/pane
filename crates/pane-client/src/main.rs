//! `pane` — native GPU-accelerated remote workspace over SSH stdio.

mod app;
mod files_view;
mod metrics;
mod monitor_view;
mod processes_view;
mod terminal_view;
mod theme;
mod transport;
mod viewer_view;

use anyhow::{Context, Result};
use clap::Parser;

const DEFAULT_TARGET: &str = "rutytoi@100.100.33.5";

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
}

fn main() -> Result<()> {
    let args = Args::parse();

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
            Ok(Box::new(app::PaneApp::new(cc.egui_ctx.clone(), mode)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
    .context("failed to start native GPU workspace")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_defaults_to_default_target() {
        let a = Args::try_parse_from(["pane"]).unwrap();
        assert!(!a.demo);
        assert_eq!(a.target, DEFAULT_TARGET);
        assert_eq!(a.host_bin, "pane-host");
    }

    #[test]
    fn cli_allows_demo_and_custom_target_override() {
        let demo = Args::try_parse_from(["pane", "--demo"]).unwrap();
        assert!(demo.demo);

        let custom = Args::try_parse_from(["pane", "otheruser@otherhost", "--host-bin", "/opt/pane-host"]).unwrap();
        assert!(!custom.demo);
        assert_eq!(custom.target, "otheruser@otherhost");
        assert_eq!(custom.host_bin, "/opt/pane-host");
    }
}
