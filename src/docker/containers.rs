use bollard::models::{ContainerCreateBody, ContainerSummary, HostConfig, RestartPolicy, RestartPolicyNameEnum};
use std::collections::HashMap;

use crate::config::types::ServiceConfig;
use super::labels;

/// Represents a running or stopped korgi-managed container.
#[derive(Debug, Clone)]
pub struct KorgiContainer {
    pub id: String,
    pub name: String,
    pub host_name: String,
    pub service: String,
    pub generation: u64,
    pub instance: u32,
    pub image: String,
    pub state: String,
    pub status: String,
    pub health: Option<String>,
}

impl KorgiContainer {
    /// Parse a ContainerSummary (from docker list) into a KorgiContainer.
    pub fn from_summary(summary: &ContainerSummary, host_name: &str) -> Option<Self> {
        let container_labels = summary.labels.as_ref()?;
        let service = labels::parse_service(container_labels)?;
        let generation = labels::parse_generation(container_labels)?;
        let instance = labels::parse_instance(container_labels)?;
        let image = labels::parse_image(container_labels)
            .or_else(|| summary.image.clone())
            .unwrap_or_default();

        let name = summary
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_default();

        let state_str = summary
            .state
            .as_ref()
            .map(|s| format!("{:?}", s).to_lowercase())
            .unwrap_or_default();

        let status_str = summary.status.clone().unwrap_or_default();

        Some(Self {
            id: summary.id.clone().unwrap_or_default(),
            name,
            host_name: host_name.to_string(),
            service,
            generation,
            instance,
            image,
            state: state_str,
            status: status_str.clone(),
            health: extract_health_from_status(Some(&status_str)),
        })
    }
}

/// Extract health status from the docker status string (e.g., "Up 5 minutes (healthy)").
fn extract_health_from_status(status: Option<&str>) -> Option<String> {
    let status = status?;
    if status.contains("(healthy)") {
        Some("healthy".to_string())
    } else if status.contains("(unhealthy)") {
        Some("unhealthy".to_string())
    } else if status.contains("(health: starting)") {
        Some("starting".to_string())
    } else {
        None
    }
}

/// Build a Docker container config from a korgi service config.
pub fn build_container_config(
    project: &str,
    svc: &ServiceConfig,
    generation: u64,
    instance: u32,
    traefik_network: &str,
    resolved_env: &HashMap<String, String>,
) -> ContainerCreateBody {
    let container_labels = labels::all_labels(project, svc, generation, instance, traefik_network);

    let env: Vec<String> = resolved_env
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect();

    let healthcheck = svc.health.as_ref().map(|h| {
        let port = svc.ports.as_ref().map(|p| p.container).unwrap_or(80);
        bollard::models::HealthConfig {
            test: Some(vec![
                "CMD-SHELL".to_string(),
                format!("wget -q --spider http://localhost:{}{} || exit 1", port, h.path),
            ]),
            interval: Some(parse_duration_ns(&h.interval)),
            timeout: Some(parse_duration_ns(&h.timeout)),
            retries: Some(h.retries as i64),
            start_period: h.start_period.as_ref().map(|sp| parse_duration_ns(sp)),
            start_interval: None,
        }
    });

    let mut binds = Vec::new();
    for vol in &svc.volumes {
        let mount = if vol.readonly {
            format!("{}:{}:ro", vol.host, vol.container)
        } else {
            format!("{}:{}", vol.host, vol.container)
        };
        binds.push(mount);
    }

    let restart_policy = Some(RestartPolicy {
        name: Some(match svc.restart.as_str() {
            "no" => RestartPolicyNameEnum::NO,
            "always" => RestartPolicyNameEnum::ALWAYS,
            "on-failure" => RestartPolicyNameEnum::ON_FAILURE,
            _ => RestartPolicyNameEnum::UNLESS_STOPPED,
        }),
        maximum_retry_count: None,
    });

    let memory = svc
        .resources
        .as_ref()
        .and_then(|r| r.memory.as_ref())
        .map(|m| parse_memory_bytes(m));

    let nano_cpus = svc
        .resources
        .as_ref()
        .and_then(|r| r.cpus.as_ref())
        .map(|c| parse_nano_cpus(c));

    let host_config = HostConfig {
        binds: if binds.is_empty() {
            None
        } else {
            Some(binds)
        },
        restart_policy,
        memory,
        nano_cpus,
        network_mode: Some(traefik_network.to_string()),
        ..Default::default()
    };

    ContainerCreateBody {
        image: Some(svc.image.clone()),
        labels: Some(container_labels),
        env: if env.is_empty() { None } else { Some(env) },
        cmd: svc.command.clone(),
        entrypoint: svc.entrypoint.clone(),
        healthcheck,
        host_config: Some(host_config),
        ..Default::default()
    }
}

/// Parse a duration string like "5s", "30s", "1m" into nanoseconds.
fn parse_duration_ns(s: &str) -> i64 {
    let s = s.trim();
    if let Some(secs) = s.strip_suffix('s') {
        secs.parse::<i64>().unwrap_or(5) * 1_000_000_000
    } else if let Some(mins) = s.strip_suffix('m') {
        mins.parse::<i64>().unwrap_or(1) * 60 * 1_000_000_000
    } else {
        s.parse::<i64>().unwrap_or(5) * 1_000_000_000
    }
}

/// Parse a memory string like "512m", "1g" into bytes.
fn parse_memory_bytes(s: &str) -> i64 {
    let s = s.trim().to_lowercase();
    if let Some(mb) = s.strip_suffix('m') {
        mb.parse::<i64>().unwrap_or(512) * 1024 * 1024
    } else if let Some(gb) = s.strip_suffix('g') {
        gb.parse::<i64>().unwrap_or(1) * 1024 * 1024 * 1024
    } else if let Some(kb) = s.strip_suffix('k') {
        kb.parse::<i64>().unwrap_or(256) * 1024
    } else {
        s.parse::<i64>().unwrap_or(536870912)
    }
}

/// Parse a CPU string like "1.0", "0.5" into nano CPUs.
fn parse_nano_cpus(s: &str) -> i64 {
    let cpus: f64 = s.parse().unwrap_or(1.0);
    (cpus * 1_000_000_000.0) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_ns() {
        assert_eq!(parse_duration_ns("5s"), 5_000_000_000);
        assert_eq!(parse_duration_ns("1m"), 60_000_000_000);
        assert_eq!(parse_duration_ns("30s"), 30_000_000_000);
    }

    #[test]
    fn test_parse_memory_bytes() {
        assert_eq!(parse_memory_bytes("512m"), 536870912);
        assert_eq!(parse_memory_bytes("1g"), 1073741824);
        assert_eq!(parse_memory_bytes("256k"), 262144);
    }

    #[test]
    fn test_parse_nano_cpus() {
        assert_eq!(parse_nano_cpus("1.0"), 1_000_000_000);
        assert_eq!(parse_nano_cpus("0.5"), 500_000_000);
        assert_eq!(parse_nano_cpus("2.0"), 2_000_000_000);
    }
}
