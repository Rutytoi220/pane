//! 1 Hz system telemetry from `/proc/stat`, `/proc/meminfo`, `/proc/loadavg`
//! and `/proc/uptime`.

use std::fs;
use std::time::Duration;

use pane_proto::HostMsg;

use crate::outbox::Outbox;

pub const INTERVAL: Duration = Duration::from_millis(1000);

/// Cumulative jiffy counters of one `cpu` line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub idle: u64,
    pub total: u64,
}

impl CpuTimes {
    /// Utilisation in percent between two samples.
    pub fn usage_since(&self, prev: &CpuTimes) -> f32 {
        let total = self.total.saturating_sub(prev.total);
        let idle = self.idle.saturating_sub(prev.idle);
        if total == 0 { 0.0 } else { ((total - idle.min(total)) as f32 / total as f32 * 100.0).clamp(0.0, 100.0) }
    }
}

/// Parse `/proc/stat`. Returns the aggregate line followed by one entry per core.
pub fn parse_proc_stat(s: &str) -> Option<(CpuTimes, Vec<CpuTimes>)> {
    let mut aggregate = None;
    let mut cores = Vec::new();
    for line in s.lines() {
        let mut it = line.split_whitespace();
        let Some(label) = it.next() else { continue };
        if !label.starts_with("cpu") {
            continue;
        }
        let fields: Vec<u64> = it.filter_map(|f| f.parse().ok()).collect();
        if fields.len() < 4 {
            continue;
        }
        // user nice system idle iowait irq softirq steal [guest guest_nice]
        // guest time is already accounted in user/nice, so only sum the first 8.
        let total: u64 = fields.iter().take(8).sum();
        let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
        let t = CpuTimes { idle, total };
        if label == "cpu" { aggregate = Some(t) } else { cores.push(t) }
    }
    aggregate.map(|a| (a, cores))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemInfo {
    pub total_kb: u64,
    pub available_kb: u64,
}

pub fn parse_meminfo(s: &str) -> Option<MemInfo> {
    let field = |name: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|rest| rest.trim_start_matches(':').split_whitespace().next())
            .and_then(|v| v.parse::<u64>().ok())
    };
    let total_kb = field("MemTotal")?;
    let available_kb = field("MemAvailable").or_else(|| {
        // Pre-3.14 kernels: approximate.
        Some(field("MemFree")? + field("Buffers").unwrap_or(0) + field("Cached").unwrap_or(0))
    })?;
    Some(MemInfo { total_kb, available_kb })
}

pub fn parse_loadavg(s: &str) -> [f32; 3] {
    let mut it = s.split_whitespace().map(|v| v.parse::<f32>().unwrap_or(0.0));
    [it.next().unwrap_or(0.0), it.next().unwrap_or(0.0), it.next().unwrap_or(0.0)]
}

pub fn spawn(outbox: Outbox) {
    std::thread::Builder::new()
        .name("telemetry".into())
        .spawn(move || {
            let mut prev = fs::read_to_string("/proc/stat").ok().and_then(|s| parse_proc_stat(&s));
            if prev.is_none() {
                log!("/proc/stat unavailable; CPU telemetry disabled");
            }
            loop {
                std::thread::sleep(INTERVAL);
                let cur = fs::read_to_string("/proc/stat").ok().and_then(|s| parse_proc_stat(&s));
                let (cpu_percent, per_core) = match (&prev, &cur) {
                    (Some((pa, pc)), Some((ca, cc))) => (
                        ca.usage_since(pa),
                        cc.iter().zip(pc.iter()).map(|(c, p)| c.usage_since(p)).collect(),
                    ),
                    _ => (0.0, Vec::new()),
                };
                prev = cur;

                let mem_used_mb = fs::read_to_string("/proc/meminfo")
                    .ok()
                    .and_then(|s| parse_meminfo(&s))
                    .map(|m| m.total_kb.saturating_sub(m.available_kb) / 1024)
                    .unwrap_or(0);
                let load_avg = fs::read_to_string("/proc/loadavg").map(|s| parse_loadavg(&s)).unwrap_or_default();
                let uptime_secs = fs::read_to_string("/proc/uptime")
                    .ok()
                    .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
                    .map(|v| v as u64)
                    .unwrap_or(0);

                outbox.send(HostMsg::TelemetryUpdate { cpu_percent, mem_used_mb, load_avg, per_core, uptime_secs });
            }
        })
        .expect("spawn telemetry thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT_A: &str = "cpu  100 0 100 800 0 0 0 0 0 0\n\
                          cpu0 50 0 50 400 0 0 0 0 0 0\n\
                          cpu1 50 0 50 400 0 0 0 0 0 0\n\
                          intr 12345\nctxt 999\n";
    const STAT_B: &str = "cpu  200 0 200 1000 0 0 0 0 0 0\n\
                          cpu0 150 0 150 400 0 0 0 0 0 0\n\
                          cpu1 50 0 50 600 0 0 0 0 0 0\n";

    #[test]
    fn cpu_usage_between_samples() {
        let (a, ac) = parse_proc_stat(STAT_A).unwrap();
        let (b, bc) = parse_proc_stat(STAT_B).unwrap();
        assert_eq!(ac.len(), 2);
        assert!((b.usage_since(&a) - 50.0).abs() < 0.01);
        assert!((bc[0].usage_since(&ac[0]) - 100.0).abs() < 0.01);
        assert!((bc[1].usage_since(&ac[1]) - 0.0).abs() < 0.01);
    }

    #[test]
    fn meminfo_and_loadavg() {
        let m = parse_meminfo("MemTotal:       32768000 kB\nMemFree: 1000 kB\nMemAvailable:   16384000 kB\n").unwrap();
        assert_eq!(m, MemInfo { total_kb: 32_768_000, available_kb: 16_384_000 });
        assert_eq!(parse_loadavg("0.52 0.58 0.59 1/1046 12345\n"), [0.52, 0.58, 0.59]);
    }

    #[test]
    fn real_proc_parses_on_linux() {
        if cfg!(target_os = "linux") {
            assert!(parse_proc_stat(&fs::read_to_string("/proc/stat").unwrap()).is_some());
            assert!(parse_meminfo(&fs::read_to_string("/proc/meminfo").unwrap()).is_some());
        }
    }
}
