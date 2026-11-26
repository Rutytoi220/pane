# pane

**A zero-daemon, retained-mode remote desktop workspace over SSH stdio.**

Instead of encoding and streaming millions of pixels per second like VNC or RDP, `pane` runs a tiny, headless Rust agent (`pane-host`) directly over `ssh -T user@host pane-host` with **zero GUI, X11, Wayland, or GPU dependencies**. The remote host maintains semantic workspace state (virtual terminal grids, directory listings, and `/proc` telemetry) and streams compact, LZ4-compressed binary state diffs over SSH `stdin`/`stdout`. Your local machine renders a native, GPU-accelerated multi-window workspace (`egui` + `wgpu`) at full monitor refresh rate.

---

## Architecture: Pixel Streaming vs. `pane`

```text
  Traditional VNC / RDP (Pixel Framebuffer Streaming)
  ┌──────────────────────────────────┐               ┌────────────────────────────┐
  │ Remote Host                      │   ~12 MB/s    │ Local Viewer               │
  │  X11/Wayland + GPU/llvmpipe      │  (1080p60)    │  Decode video/tiles        │
  │  Render desktop -> capture RGB   │ ════════════> │  Blit blurry bitmap        │
  │  Background daemon + open port   │ <──────────── │  Send raw mouse/key events │
  └──────────────────────────────────┘               └────────────────────────────┘

  pane (Retained-Mode State Diffs over SSH stdio)
  ┌──────────────────────────────────┐               ┌────────────────────────────┐
  │ Remote Host (`pane-host`)        │   ~1-10 KB/s  │ Local Client (`pane`)      │
  │  Zero daemon · Zero GUI/GPU deps │  postcard+LZ4 │  Native GPU (eframe/wgpu)  │
  │  - portable-pty + vt100 grids    │ ────────────> │  - Full refresh rate UI    │
  │  - ~60 Hz dirty-row state diffs  │  SSH stdout   │  - Crisp native typography │
  │  - 64 KB chunked file reader     │               │  - Multi-window terminals  │
  │  - 1 Hz /proc telemetry          │ <──────────── │  - File explorer + monitor │
  │  - Logs strictly to stderr       │  SSH stdin    │  - Live RTT & VNC savings  │
  └──────────────────────────────────┘               └────────────────────────────┘
```

### Wire Protocol (`pane-proto`)

Every message on `stdin`/`stdout` uses a deterministic binary frame envelope:

```text
+-----------------------+-----------+-----------------------------------+
| u32 LE payload length | u8 flags  | payload (postcard, optional LZ4)  |
+-----------------------+-----------+-----------------------------------+
                         bit 0 = 1 -> LZ4 size-prepended block (>128 B)
```

- **Strict stdio hygiene**: `pane-host` reserves `stdout` exclusively for binary `HostMsg` frames; all diagnostics go to `stderr`.
- **Prioritized host outbox**: latency-sensitive control messages (`Pong`, `TermDiff`, `TelemetryUpdate`) bypass the bounded bulk queue used by 64 KiB `FileChunk` transfers so typing stays responsive during downloads.
- **Coalesced 60 Hz diffing**: each PTY feeds a headless `vt100` parser on the host; every ~16 ms, only rows whose text or SGR attributes changed since the previous tick are run-length encoded into `Span`s and transmitted.

---

## Workspace Crates

| Crate | Purpose |
|---|---|
| `crates/pane-proto` | Shared binary framing (`postcard` + `lz4_flex`), `ClientMsg`/`HostMsg` types, and the retained-mode `RowBuilder` / `diff_rows` / `Screen` diff engine. |
| `crates/pane-host` | Headless remote agent with zero X11/Wayland/GUI dependencies. Multiplexed `portable-pty` shells + `vt100` emulation, directory listings, 64 KiB file streaming, and 1 Hz Linux `/proc` telemetry. |
| `crates/pane-client` | Native GPU workspace (`eframe` + `wgpu`) featuring a live telemetry/VNC-savings HUD bar, multiple floating terminal windows, dockable/floating File Explorer (with one-click download to `~/Downloads`), and System Monitor. |

---

## Quickstart

### Build & Test

```bash
cargo test --workspace
cargo build --workspace
```

### Offline Demo Mode (zero network setup)

Spawns a local `pane-host` child process over stdio pipes so you can test the full workspace immediately:

```bash
cargo run --bin pane -- --demo
```

### Connect to a Remote Host over SSH

1. Build and copy the headless `pane-host` binary onto the remote Linux server (somewhere in `$PATH`, e.g. `~/.local/bin/pane-host`):

```bash
cargo build --release -p pane-host
scp target/release/pane-host user@host:~/.local/bin/pane-host
```

2. Launch the local GPU workspace over SSH:

```bash
cargo run --release --bin pane -- user@host
```

If `pane-host` lives at a non-standard remote path, pass `--host-bin`:

```bash
pane user@host --host-bin /opt/bin/pane-host
```

Extra SSH options can be passed through with repeatable `--ssh-opt`:

```bash
pane user@host --ssh-opt Port=2222 --ssh-opt Compression=no
```
