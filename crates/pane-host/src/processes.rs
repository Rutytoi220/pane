//! Remote process inspection (`/proc`) and signaling (`kill`).

use std::collections::HashMap;
use std::fs;

use pane_proto::ProcessInfo;

const MAX_REPORTED_PROCESSES: usize = 100;

pub struct ProcessMonitor {
    prev_total_jiffies: Option<u64>,
    prev_proc_jiffies: HashMap<u32, u64>,
    page_size_bytes: u64,
    cpu_cores: f32,
}

impl ProcessMonitor {
    pub fn new() -> Self {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page_size_bytes = if page_size > 0 { page_size as u64 } else { 4096 };
        let cpu_cores = std::thread::available_parallelism().map(|n| n.get() as f32).unwrap_or(1.0);
        let initial_total = read_total_jiffies();
        Self {
            prev_total_jiffies: initial_total,
            prev_proc_jiffies: HashMap::new(),
            page_size_bytes,
            cpu_cores,
        }
    }

    /// Snapshot top running processes by CPU % and RSS memory from `/proc`.
    pub fn snapshot(&mut self) -> Vec<ProcessInfo> {
        let cur_total = read_total_jiffies();
        let delta_total = match (self.prev_total_jiffies, cur_total) {
            (Some(prev), Some(cur)) => cur.saturating_sub(prev),
            _ => 0,
        };
        self.prev_total_jiffies = cur_total;

        let Ok(entries) = fs::read_dir("/proc") else { return Vec::new() };
        let mut next_jiffies = HashMap::new();
        let mut out = Vec::new();

        for entry in entries.filter_map(Result::ok) {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
            let Ok(pid) = name.parse::<u32>() else { continue };

            let stat_path = entry.path().join("stat");
            let Ok(stat_str) = fs::read_to_string(stat_path) else { continue };
            let Some(parsed) = parse_proc_pid_stat(&stat_str) else { continue };

            next_jiffies.insert(pid, parsed.cpu_jiffies);
            let cpu_percent = if delta_total > 0 {
                if let Some(&prev_j) = self.prev_proc_jiffies.get(&pid) {
                    let delta_p = parsed.cpu_jiffies.saturating_sub(prev_j);
                    ((delta_p as f32 / delta_total as f32) * 100.0 * self.cpu_cores)
                        .clamp(0.0, self.cpu_cores * 100.0)
                } else {
                    0.0
                }
            } else {
                0.0
            };

            let rss_pages = fs::read_to_string(entry.path().join("statm"))
                .ok()
                .and_then(|s| parse_statm_rss_pages(&s))
                .unwrap_or(parsed.rss_pages);
            let rss_mb = (rss_pages * self.page_size_bytes) as f32 / (1024.0 * 1024.0);

            out.push(ProcessInfo {
                pid,
                name: parsed.comm,
                cpu_percent,
                rss_mb,
                state: parsed.state.to_string(),
            });
        }

        self.prev_proc_jiffies = next_jiffies;

        out.sort_by(|a, b| {
            b.cpu_percent
                .total_cmp(&a.cpu_percent)
                .then_with(|| b.rss_mb.total_cmp(&a.rss_mb))
                .then_with(|| a.pid.cmp(&b.pid))
        });
        out.truncate(MAX_REPORTED_PROCESSES);
        out
    }
}

/// Send a POSIX signal to `pid`. Refuses PID 0/1 and out-of-range signals.
pub fn kill_process(pid: u32, signal: i32) -> Result<(), String> {
    if pid <= 1 {
        return Err(format!("refusing to signal protected pid {pid}"));
    }
    if !(1..=64).contains(&signal) {
        return Err(format!("invalid signal {signal}"));
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if rc == 0 {
        log!("sent signal {signal} to pid {pid}");
        Ok(())
    } else {
        let err = std::io::Error::last_os_error();
        Err(format!("kill({pid}, {signal}) failed: {err}"))
    }
}

fn read_total_jiffies() -> Option<u64> {
    let s = fs::read_to_string("/proc/stat").ok()?;
    crate::telemetry::parse_proc_stat(&s).map(|(agg, _)| agg.total)
}

#[derive(Debug, PartialEq, Eq)]
struct PidStat {
    comm: String,
    state: char,
    cpu_jiffies: u64,
    rss_pages: u64,
}

/// Parse `/proc/<pid>/stat`, safely handling process names containing spaces or
/// parentheses (by locating the last `)`).
fn parse_proc_pid_stat(s: &str) -> Option<PidStat> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    if close <= open {
        return None;
    }
    let comm = s[open + 1..close].to_string();
    let rest = s.get(close + 2..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After `(comm) `, index 0 is state (field 3), utime is field 14 -> index 11,
    // stime is field 15 -> index 12, rss is field 24 -> index 21.
    if fields.len() < 22 {
        return None;
    }
    let state = fields[0].chars().next().unwrap_or('?');
    let utime: u64 = fields[11].parse().ok()?;
    let stime: u64 = fields[12].parse().ok()?;
    let rss_pages: u64 = fields[21].parse().unwrap_or(0);
    Some(PidStat { comm, state, cpu_jiffies: utime.saturating_add(stime), rss_pages })
}

fn parse_statm_rss_pages(s: &str) -> Option<u64> {
    s.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_stat_with_spaces_and_parens_in_name() {
        let line = "4242 (Web (Content) 1) S 1 4242 4242 0 -1 4194304 100 0 0 0 120 30 0 0 20 0 8 0 999 500000 2048 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0";
        let st = parse_proc_pid_stat(line).unwrap();
        assert_eq!(st.comm, "Web (Content) 1");
        assert_eq!(st.state, 'S');
        assert_eq!(st.cpu_jiffies, 150);
        assert_eq!(st.rss_pages, 2048);
    }

    #[test]
    fn lists_real_processes_on_linux() {
        if cfg!(target_os = "linux") {
            let mut mon = ProcessMonitor::new();
            let list = mon.snapshot();
            assert!(!list.is_empty());
            assert!(list.iter().all(|p| p.pid > 0 && !p.name.is_empty()));
        }
    }

    #[test]
    fn refuses_to_kill_pid_1() {
        assert!(kill_process(1, 15).is_err());
        assert!(kill_process(0, 9).is_err());
    }
}
