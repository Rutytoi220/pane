//! Remote systemd service & Docker container inspection and control.
//!
//! Runs `systemctl` and `docker` on background worker threads so slow daemon
//! operations never stall terminal input or frame I/O.

use std::process::Command;

use pane_proto::{ContainerInfo, HostMsg, ServiceAction, ServiceInfo};

use crate::outbox::Outbox;

const MAX_SERVICES: usize = 120;

pub fn spawn_list(req_id: u64, outbox: Outbox) {
    std::thread::Builder::new()
        .name(format!("svc-list-{req_id}"))
        .spawn(move || {
            let (services, systemd_available) = list_systemd_services();
            let (containers, docker_available) = list_docker_containers();
            outbox.send(HostMsg::ServiceSnapshot {
                req_id,
                services,
                containers,
                systemd_available,
                docker_available,
            });
        })
        .expect("spawn service list thread");
}

pub fn spawn_control_service(req_id: u64, name: String, action: ServiceAction, outbox: Outbox) {
    std::thread::Builder::new()
        .name(format!("svc-ctl-{req_id}"))
        .spawn(move || {
            let result = control_systemd_service(&name, action);
            let (ok, message) = match result {
                Ok(msg) => (true, msg),
                Err(err) => (false, err),
            };
            outbox.send(HostMsg::ControlResult { req_id, target: name, ok, message });
            // Push a fresh snapshot right away so the UI updates.
            let (services, systemd_available) = list_systemd_services();
            let (containers, docker_available) = list_docker_containers();
            outbox.send(HostMsg::ServiceSnapshot {
                req_id,
                services,
                containers,
                systemd_available,
                docker_available,
            });
        })
        .expect("spawn service control thread");
}

pub fn spawn_control_container(req_id: u64, id: String, action: ServiceAction, outbox: Outbox) {
    std::thread::Builder::new()
        .name(format!("docker-ctl-{req_id}"))
        .spawn(move || {
            let result = control_docker_container(&id, action);
            let (ok, message) = match result {
                Ok(msg) => (true, msg),
                Err(err) => (false, err),
            };
            outbox.send(HostMsg::ControlResult { req_id, target: id, ok, message });
            let (services, systemd_available) = list_systemd_services();
            let (containers, docker_available) = list_docker_containers();
            outbox.send(HostMsg::ServiceSnapshot {
                req_id,
                services,
                containers,
                systemd_available,
                docker_available,
            });
        })
        .expect("spawn container control thread");
}

fn list_systemd_services() -> (Vec<ServiceInfo>, bool) {
    let output = Command::new("systemctl")
        .args(["list-units", "--type=service", "--all", "--no-pager", "--no-legend", "--plain"])
        .output();
    let Ok(out) = output else {
        return (Vec::new(), false);
    };
    if !out.status.success() && out.stdout.is_empty() {
        return (Vec::new(), false);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut services = parse_systemctl_output(&text);
    // Prioritize running / failed / activating services at the top.
    services.sort_by(|a, b| {
        service_priority(a)
            .cmp(&service_priority(b))
            .then_with(|| a.name.cmp(&b.name))
    });
    services.truncate(MAX_SERVICES);
    (services, true)
}

fn service_priority(s: &ServiceInfo) -> u8 {
    match (s.active_state.as_str(), s.sub_state.as_str()) {
        ("failed", _) => 0,
        ("active", "running") => 1,
        ("activating", _) | ("reloading", _) => 2,
        ("active", _) => 3,
        _ => 4,
    }
}

pub fn parse_systemctl_output(s: &str) -> Vec<ServiceInfo> {
    let mut out = Vec::new();
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(unit) = parts.next() else { continue };
        let Some(_load) = parts.next() else { continue };
        let Some(active) = parts.next() else { continue };
        let Some(sub) = parts.next() else { continue };
        let description = parts.collect::<Vec<_>>().join(" ");
        out.push(ServiceInfo {
            name: unit.to_owned(),
            active_state: active.to_owned(),
            sub_state: sub.to_owned(),
            description,
        });
    }
    out
}

fn list_docker_containers() -> (Vec<ContainerInfo>, bool) {
    let output = Command::new("docker")
        .args(["ps", "-a", "--format", "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.State}}"])
        .output();
    let Ok(out) = output else {
        return (Vec::new(), false);
    };
    if !out.status.success() {
        return (Vec::new(), false);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    (parse_docker_ps_output(&text), true)
}

pub fn parse_docker_ps_output(s: &str) -> Vec<ContainerInfo> {
    s.lines()
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let id = cols.next()?.trim().to_owned();
            if id.is_empty() {
                return None;
            }
            let name = cols.next().unwrap_or("").trim().to_owned();
            let image = cols.next().unwrap_or("").trim().to_owned();
            let status = cols.next().unwrap_or("").trim().to_owned();
            let state = cols.next().unwrap_or("").trim().to_owned();
            Some(ContainerInfo { id, name, image, status, state })
        })
        .collect()
}

fn valid_unit_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.len() <= 256
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-' | ':'))
}

fn control_systemd_service(name: &str, action: ServiceAction) -> Result<String, String> {
    if !valid_unit_name(name) {
        return Err(format!("invalid service unit name: {name:?}"));
    }
    let verb = action.as_str();
    let out = Command::new("systemctl")
        .args([verb, name])
        .output()
        .map_err(|e| format!("failed to run systemctl: {e}"))?;
    if out.status.success() {
        log!("systemctl {verb} {name}: ok");
        Ok(format!("systemctl {verb} {name} succeeded"))
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if stderr.is_empty() {
            format!("systemctl {verb} {name} failed ({})", out.status)
        } else {
            format!("systemctl {verb} {name}: {stderr}")
        })
    }
}

fn control_docker_container(id: &str, action: ServiceAction) -> Result<String, String> {
    if !valid_unit_name(id) {
        return Err(format!("invalid container id: {id:?}"));
    }
    let verb = action.as_str();
    let out = Command::new("docker")
        .args([verb, id])
        .output()
        .map_err(|e| format!("failed to run docker: {e}"))?;
    if out.status.success() {
        log!("docker {verb} {id}: ok");
        Ok(format!("docker {verb} {id} succeeded"))
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if stderr.is_empty() {
            format!("docker {verb} {id} failed ({})", out.status)
        } else {
            format!("docker {verb} {id}: {stderr}")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_systemctl_and_docker_outputs() {
        let sys = "ssh.service loaded active running OpenBSD Secure Shell server\n\
                   nginx.service loaded failed failed A high performance web server\n";
        let svcs = parse_systemctl_output(sys);
        assert_eq!(svcs.len(), 2);
        assert_eq!(svcs[0].name, "ssh.service");
        assert_eq!(svcs[0].sub_state, "running");
        assert_eq!(svcs[1].active_state, "failed");

        let dock = "a1b2c3d4\tweb-api\tnginx:alpine\tUp 3 hours\trunning\n\
                    e5f6g7h8\tdb\tpostgres:16\tExited (0) 2 minutes ago\texited\n";
        let ctrs = parse_docker_ps_output(dock);
        assert_eq!(ctrs.len(), 2);
        assert_eq!(ctrs[0].name, "web-api");
        assert_eq!(ctrs[0].state, "running");
        assert_eq!(ctrs[1].image, "postgres:16");
    }

    #[test]
    fn rejects_unsafe_unit_names() {
        assert!(valid_unit_name("nginx.service"));
        assert!(valid_unit_name("user@1000.service"));
        assert!(!valid_unit_name("--root=/"));
        assert!(!valid_unit_name("foo; rm -rf /"));
    }
}
