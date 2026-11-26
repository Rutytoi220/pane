//! Live connection metrics: RTT latency, sliding-window wire bandwidth,
//! total transferred bytes, VNC baseline comparison, and sparkline histories.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Realistic 1080p @ 60 Hz VNC framebuffer stream baseline: 12 MB/s.
pub const VNC_1080P60_BYTES_PER_SEC: f64 = 12.0 * 1_000_000.0;

const RATE_WINDOW: Duration = Duration::from_secs(2);
const HISTORY_LEN: usize = 60;

#[derive(Debug)]
pub struct SessionMetrics {
    started_at: Instant,
    samples: VecDeque<(Instant, usize)>,
    pub total_rx_bytes: u64,
    pub total_tx_bytes: u64,
    pub frames_rx: u64,
    pub frames_tx: u64,
    pub rtt_ms: Option<f64>,
    pub last_ping_sent: Option<Instant>,

    // 1 Hz history buffers for System Monitor charts.
    pub cpu_history: VecDeque<f32>,
    pub mem_history: VecDeque<f32>,
    pub bw_kbps_history: VecDeque<f32>,
    last_bw_sample: Instant,
}

impl SessionMetrics {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            started_at: now,
            samples: VecDeque::new(),
            total_rx_bytes: 0,
            total_tx_bytes: 0,
            frames_rx: 0,
            frames_tx: 0,
            rtt_ms: None,
            last_ping_sent: None,
            cpu_history: VecDeque::with_capacity(HISTORY_LEN),
            mem_history: VecDeque::with_capacity(HISTORY_LEN),
            bw_kbps_history: VecDeque::with_capacity(HISTORY_LEN),
            last_bw_sample: now,
        }
    }

    pub fn record_rx(&mut self, bytes: usize) {
        let now = Instant::now();
        self.total_rx_bytes += bytes as u64;
        self.frames_rx += 1;
        self.samples.push_back((now, bytes));
        self.prune(now);
    }

    pub fn record_tx(&mut self, bytes: usize) {
        let now = Instant::now();
        self.total_tx_bytes += bytes as u64;
        self.frames_tx += 1;
        self.samples.push_back((now, bytes));
        self.prune(now);
    }

    pub fn record_pong(&mut self, sent_micros: u64) {
        let now_micros = self.elapsed_micros();
        if now_micros >= sent_micros {
            let rtt = (now_micros - sent_micros) as f64 / 1000.0;
            self.rtt_ms = Some(match self.rtt_ms {
                Some(prev) => prev * 0.4 + rtt * 0.6,
                None => rtt,
            });
        }
    }

    pub fn record_telemetry(&mut self, cpu_percent: f32, mem_percent: f32) {
        push_bounded(&mut self.cpu_history, cpu_percent.clamp(0.0, 100.0));
        push_bounded(&mut self.mem_history, mem_percent.clamp(0.0, 100.0));
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        self.prune(now);
        if now.duration_since(self.last_bw_sample) >= Duration::from_secs(1) {
            self.last_bw_sample = now;
            let kbps = self.bandwidth_kb_per_sec() as f32;
            push_bounded(&mut self.bw_kbps_history, kbps);
        }
    }

    pub fn elapsed_micros(&self) -> u64 {
        self.started_at.elapsed().as_micros() as u64
    }

    pub fn elapsed_secs(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_rx_bytes + self.total_tx_bytes
    }

    /// Real-time compressed wire bandwidth in KB/s over the trailing window.
    pub fn bandwidth_kb_per_sec(&self) -> f64 {
        let sum: usize = self.samples.iter().map(|(_, b)| *b).sum();
        let secs = self.elapsed_secs().clamp(0.25, RATE_WINDOW.as_secs_f64());
        (sum as f64 / secs) / 1024.0
    }

    /// Cumulative bytes a 1080p60 VNC session (12 MB/s) would have sent by now.
    pub fn vnc_equivalent_bytes(&self) -> f64 {
        self.elapsed_secs() * VNC_1080P60_BYTES_PER_SEC
    }

    /// Bytes saved relative to 1080p60 VNC.
    pub fn bytes_saved_vs_vnc(&self) -> f64 {
        (self.vnc_equivalent_bytes() - self.total_bytes() as f64).max(0.0)
    }

    /// Percentage of bandwidth saved vs 1080p60 VNC.
    pub fn saved_percent_vs_vnc(&self) -> f64 {
        let vnc = self.vnc_equivalent_bytes();
        if vnc <= 1.0 { 100.0 } else { (self.bytes_saved_vs_vnc() / vnc * 100.0).clamp(0.0, 100.0) }
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&(t, _)) = self.samples.front() {
            if now.duration_since(t) > RATE_WINDOW {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }
}

fn push_bounded(buf: &mut VecDeque<f32>, val: f32) {
    if buf.len() == HISTORY_LEN {
        buf.pop_front();
    }
    buf.push_back(val);
}

/// Format a byte count into a crisp human-readable string (B / KB / MB / GB).
pub fn fmt_bytes(bytes: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    if bytes >= GB {
        format!("{:.2} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{:.0} B", bytes)
    }
}

pub fn fmt_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours:02}h {mins:02}m")
    } else {
        format!("{hours:02}h {mins:02}m")
    }
}
