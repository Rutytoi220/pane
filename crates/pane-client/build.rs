//! Build script for `pane-client`: reads the musl-pane-host binary at compile time so its bytes are captured by `include_bytes!`.  Tells Cargo to re-run on changes.

use std::env;
use std::path::Path;

fn main() {
    // CARGO_MANIFEST_DIR ends in `Cargo.toml`; strip it to get the crate dir.
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap_or_else(|| Path::new(""));

    // Walk up from the crate dir looking for a directory that already has a `target/` child.
    let mut work = manifest_dir.to_path_buf();
    for _ in 0..6 {
        if work.join("target").is_dir() {
            break;
        }
        let Some(parent) = work.parent().filter(|p| p != &work && !p.as_os_str().is_empty()) else {
            break;
        };
        work = parent.to_path_buf();
    }

    let musl_bin = work.join("target/x86_64-unknown-linux-musl/release/pane-host");

    if !musl_bin.is_file() {
        panic!(
            "Musl pane-host not found at {}\n\nBuild it first:\n  cargo build --release -p pane-host \\\n   --target x86_64-unknown-linux-musl",
            musl_bin.display(),
        );
    }

    println!("cargo:rerun-if-changed={}", musl_bin.display());

    // Also rebuild when this script changes.
    if let Ok(dir) = env::var("CARGO_MANIFEST_DIR") {
        let brs = Path::new(&dir).join("build.rs");
        println!("cargo:rerun-if-changed={}", brs.display());
    }
}
