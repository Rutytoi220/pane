//! Static machine description for the `Welcome` handshake.

use std::fs;

pub struct SysInfo {
    pub hostname: String,
    pub os_info: String,
    pub cpu_cores: u32,
    pub total_mem_mb: u64,
}

pub fn collect() -> SysInfo {
    let hostname = read_trimmed("/proc/sys/kernel/hostname")
        .or_else(|| read_trimmed("/etc/hostname"))
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown-host".into());

    let distro = fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| parse_os_release(&s))
        .unwrap_or_else(|| std::env::consts::OS.to_string());
    let kernel = read_trimmed("/proc/sys/kernel/osrelease");
    let os_info = match kernel {
        Some(k) => format!("{distro} · Linux {k} · {}", std::env::consts::ARCH),
        None => format!("{distro} · {}", std::env::consts::ARCH),
    };

    let cpu_cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let total_mem_mb = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| crate::telemetry::parse_meminfo(&s))
        .map(|m| m.total_kb / 1024)
        .unwrap_or(0);

    SysInfo { hostname, os_info, cpu_cores, total_mem_mb }
}

fn read_trimmed(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn parse_os_release(s: &str) -> Option<String> {
    s.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_pretty_name() {
        let s = "NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\nID=arch\n";
        assert_eq!(super::parse_os_release(s).as_deref(), Some("Arch Linux"));
    }
}
