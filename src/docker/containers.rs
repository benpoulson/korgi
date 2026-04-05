use bollard::models::{
    ContainerCreateBody, ContainerSummary, HostConfig, RestartPolicy, RestartPolicyNameEnum,
};
use std::collections::HashMap;

use super::labels;
use crate::config::types::ServiceConfig;

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
    host_bind_ip: Option<&str>,
) -> ContainerCreateBody {
    let container_labels = labels::all_labels(project, svc, generation, instance, traefik_network);

    let env: Vec<String> = resolved_env
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect();

    // Only set Docker HEALTHCHECK for mode=docker. Mode=http is checked by korgi externally.
    let healthcheck = svc.health.as_ref().and_then(|h| {
        use crate::config::types::HealthMode;
        if h.mode == HealthMode::Http {
            return None;
        }
        let port = svc.ports.as_ref().map(|p| p.container).unwrap_or(80);
        Some(bollard::models::HealthConfig {
            test: Some(vec![
                "CMD-SHELL".to_string(),
                format!(
                    "(curl -sf http://localhost:{}{} > /dev/null) || (wget -q --spider http://localhost:{}{}) || exit 1",
                    port, h.path, port, h.path
                ),
            ]),
            interval: Some(parse_duration_ns(&h.interval)),
            timeout: Some(parse_duration_ns(&h.timeout)),
            retries: Some(h.retries as i64),
            start_period: h.start_period.as_ref().map(|sp| parse_duration_ns(sp)),
            start_interval: None,
        })
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

    // Build port bindings if host_base or host port is configured
    let port_bindings = svc.ports.as_ref().and_then(|ports| {
        let host_port = if let Some(base) = ports.host_base {
            Some(base + instance as u16)
        } else {
            ports.host
        };

        host_port.map(|hp| {
            let bind_ip = host_bind_ip.unwrap_or("0.0.0.0").to_string();
            let mut bindings = HashMap::new();
            bindings.insert(
                format!("{}/tcp", ports.container),
                Some(vec![bollard::models::PortBinding {
                    host_ip: Some(bind_ip),
                    host_port: Some(hp.to_string()),
                }]),
            );
            bindings
        })
    });

    let host_config = HostConfig {
        binds: if binds.is_empty() { None } else { Some(binds) },
        port_bindings,
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
    use crate::config::types::*;
    use bollard::models::ContainerSummaryStateEnum;

    // --- Parse helpers ---

    #[test]
    fn test_parse_duration_ns() {
        assert_eq!(parse_duration_ns("5s"), 5_000_000_000);
        assert_eq!(parse_duration_ns("1m"), 60_000_000_000);
        assert_eq!(parse_duration_ns("30s"), 30_000_000_000);
    }

    #[test]
    fn test_parse_duration_ns_edge_cases() {
        assert_eq!(parse_duration_ns("0s"), 0);
        assert_eq!(parse_duration_ns("2m"), 120_000_000_000);
        // Bare number treated as seconds
        assert_eq!(parse_duration_ns("10"), 10_000_000_000);
        // With whitespace
        assert_eq!(parse_duration_ns(" 5s "), 5_000_000_000);
    }

    #[test]
    fn test_parse_memory_bytes() {
        assert_eq!(parse_memory_bytes("512m"), 536870912);
        assert_eq!(parse_memory_bytes("1g"), 1073741824);
        assert_eq!(parse_memory_bytes("256k"), 262144);
    }

    #[test]
    fn test_parse_memory_bytes_case_insensitive() {
        assert_eq!(parse_memory_bytes("512M"), 536870912);
        assert_eq!(parse_memory_bytes("1G"), 1073741824);
        assert_eq!(parse_memory_bytes("256K"), 262144);
    }

    #[test]
    fn test_parse_nano_cpus() {
        assert_eq!(parse_nano_cpus("1.0"), 1_000_000_000);
        assert_eq!(parse_nano_cpus("0.5"), 500_000_000);
        assert_eq!(parse_nano_cpus("2.0"), 2_000_000_000);
        assert_eq!(parse_nano_cpus("0.25"), 250_000_000);
    }

    // --- extract_health_from_status ---

    #[test]
    fn test_extract_health_healthy() {
        assert_eq!(
            extract_health_from_status(Some("Up 5 minutes (healthy)")),
            Some("healthy".to_string())
        );
    }

    #[test]
    fn test_extract_health_unhealthy() {
        assert_eq!(
            extract_health_from_status(Some("Up 3 minutes (unhealthy)")),
            Some("unhealthy".to_string())
        );
    }

    #[test]
    fn test_extract_health_starting() {
        assert_eq!(
            extract_health_from_status(Some("Up 1 second (health: starting)")),
            Some("starting".to_string())
        );
    }

    #[test]
    fn test_extract_health_no_health() {
        assert_eq!(extract_health_from_status(Some("Up 5 minutes")), None);
    }

    #[test]
    fn test_extract_health_none() {
        assert_eq!(extract_health_from_status(None), None);
    }

    // --- KorgiContainer::from_summary ---

    fn make_summary(
        labels: HashMap<String, String>,
        state: Option<ContainerSummaryStateEnum>,
        status: Option<String>,
    ) -> ContainerSummary {
        ContainerSummary {
            id: Some("abc123".to_string()),
            names: Some(vec!["/korgi-myapp-api-g3-0".to_string()]),
            image: Some("myapp/api:v1".to_string()),
            labels: Some(labels),
            state: state,
            status: status,
            ..Default::default()
        }
    }

    fn valid_labels() -> HashMap<String, String> {
        let mut labels = HashMap::new();
        labels.insert("korgi.project".to_string(), "myapp".to_string());
        labels.insert("korgi.service".to_string(), "api".to_string());
        labels.insert("korgi.generation".to_string(), "3".to_string());
        labels.insert("korgi.instance".to_string(), "0".to_string());
        labels.insert("korgi.image".to_string(), "myapp/api:v1".to_string());
        labels
    }

    #[test]
    fn test_from_summary_valid() {
        let summary = make_summary(
            valid_labels(),
            Some(ContainerSummaryStateEnum::RUNNING),
            Some("Up 5 minutes (healthy)".to_string()),
        );
        let container = KorgiContainer::from_summary(&summary, "web1").unwrap();
        assert_eq!(container.id, "abc123");
        assert_eq!(container.name, "korgi-myapp-api-g3-0");
        assert_eq!(container.host_name, "web1");
        assert_eq!(container.service, "api");
        assert_eq!(container.generation, 3);
        assert_eq!(container.instance, 0);
        assert_eq!(container.image, "myapp/api:v1");
        assert_eq!(container.state, "running");
        assert_eq!(container.health, Some("healthy".to_string()));
    }

    #[test]
    fn test_from_summary_stopped() {
        let summary = make_summary(
            valid_labels(),
            Some(ContainerSummaryStateEnum::EXITED),
            Some("Exited (0) 2 minutes ago".to_string()),
        );
        let container = KorgiContainer::from_summary(&summary, "web1").unwrap();
        assert_eq!(container.state, "exited");
        assert_eq!(container.health, None);
    }

    #[test]
    fn test_from_summary_no_labels() {
        let summary = ContainerSummary {
            id: Some("abc123".to_string()),
            labels: None,
            ..Default::default()
        };
        assert!(KorgiContainer::from_summary(&summary, "web1").is_none());
    }

    #[test]
    fn test_from_summary_missing_service_label() {
        let mut labels = valid_labels();
        labels.remove("korgi.service");
        let summary = make_summary(labels, Some(ContainerSummaryStateEnum::RUNNING), None);
        assert!(KorgiContainer::from_summary(&summary, "web1").is_none());
    }

    #[test]
    fn test_from_summary_missing_generation_label() {
        let mut labels = valid_labels();
        labels.remove("korgi.generation");
        let summary = make_summary(labels, Some(ContainerSummaryStateEnum::RUNNING), None);
        assert!(KorgiContainer::from_summary(&summary, "web1").is_none());
    }

    #[test]
    fn test_from_summary_invalid_generation() {
        let mut labels = valid_labels();
        labels.insert("korgi.generation".to_string(), "notanumber".to_string());
        let summary = make_summary(labels, Some(ContainerSummaryStateEnum::RUNNING), None);
        assert!(KorgiContainer::from_summary(&summary, "web1").is_none());
    }

    #[test]
    fn test_from_summary_image_fallback_to_summary() {
        let mut labels = valid_labels();
        labels.remove("korgi.image");
        let summary = make_summary(labels, Some(ContainerSummaryStateEnum::RUNNING), None);
        let container = KorgiContainer::from_summary(&summary, "web1").unwrap();
        assert_eq!(container.image, "myapp/api:v1"); // falls back to summary.image
    }

    // --- build_container_config ---

    #[test]
    fn test_build_container_config_minimal() {
        let svc = ServiceConfig::test_service("api", "myapp/api:v1");
        let env = HashMap::new();
        let config = build_container_config("myapp", &svc, 1, 0, "korgi-traefik", &env, None);

        assert_eq!(config.image, Some("myapp/api:v1".to_string()));
        assert!(config.healthcheck.is_none()); // no health config
        assert!(config.env.is_none()); // empty env
        assert!(config.cmd.is_none());
        assert!(config.entrypoint.is_none());

        let labels = config.labels.unwrap();
        assert_eq!(labels.get("korgi.project").unwrap(), "myapp");
        assert_eq!(labels.get("korgi.service").unwrap(), "api");
        assert_eq!(labels.get("korgi.generation").unwrap(), "1");

        let host_config = config.host_config.unwrap();
        assert_eq!(host_config.network_mode, Some("korgi-traefik".to_string()));
        assert!(host_config.binds.is_none()); // no volumes
    }

    #[test]
    fn test_build_container_config_with_health() {
        let mut svc = ServiceConfig::test_service("api", "api:v1");
        svc.ports = Some(PortsConfig {
            container: 3000,
            host: None,
            host_base: None,
        });
        svc.health = Some(HealthConfig {
            mode: Default::default(),
            path: "/ready".to_string(),
            interval: "10s".to_string(),
            timeout: "5s".to_string(),
            retries: 5,
            start_period: Some("15s".to_string()),
        });

        let config = build_container_config("proj", &svc, 2, 0, "net", &HashMap::new(), None);
        let hc = config.healthcheck.unwrap();
        let test = hc.test.unwrap();
        assert_eq!(test[0], "CMD-SHELL");
        assert!(test[1].contains("localhost:3000/ready"));
        assert_eq!(hc.interval, Some(10_000_000_000));
        assert_eq!(hc.timeout, Some(5_000_000_000));
        assert_eq!(hc.retries, Some(5));
        assert_eq!(hc.start_period, Some(15_000_000_000));
    }

    #[test]
    fn test_build_container_config_with_env() {
        let svc = ServiceConfig::test_service("api", "api:v1");
        let mut env = HashMap::new();
        env.insert(
            "DATABASE_URL".to_string(),
            "postgres://localhost/db".to_string(),
        );
        env.insert("REDIS_URL".to_string(), "redis://localhost".to_string());

        let config = build_container_config("proj", &svc, 1, 0, "net", &env, None);
        let env_vars = config.env.unwrap();
        assert_eq!(env_vars.len(), 2);
        assert!(env_vars.contains(&"DATABASE_URL=postgres://localhost/db".to_string()));
        assert!(env_vars.contains(&"REDIS_URL=redis://localhost".to_string()));
    }

    #[test]
    fn test_build_container_config_with_volumes() {
        let mut svc = ServiceConfig::test_service("api", "api:v1");
        svc.volumes = vec![
            VolumeConfig {
                host: "/data".to_string(),
                container: "/app/data".to_string(),
                readonly: false,
            },
            VolumeConfig {
                host: "/config".to_string(),
                container: "/app/config".to_string(),
                readonly: true,
            },
        ];

        let config = build_container_config("proj", &svc, 1, 0, "net", &HashMap::new(), None);
        let binds = config.host_config.unwrap().binds.unwrap();
        assert_eq!(binds.len(), 2);
        assert!(binds.contains(&"/data:/app/data".to_string()));
        assert!(binds.contains(&"/config:/app/config:ro".to_string()));
    }

    #[test]
    fn test_build_container_config_with_resources() {
        let mut svc = ServiceConfig::test_service("api", "api:v1");
        svc.resources = Some(ResourcesConfig {
            memory: Some("256m".to_string()),
            cpus: Some("0.5".to_string()),
        });

        let config = build_container_config("proj", &svc, 1, 0, "net", &HashMap::new(), None);
        let hc = config.host_config.unwrap();
        assert_eq!(hc.memory, Some(268435456)); // 256 * 1024 * 1024
        assert_eq!(hc.nano_cpus, Some(500_000_000));
    }

    #[test]
    fn test_build_container_config_restart_policies() {
        for (policy_str, expected) in [
            ("no", RestartPolicyNameEnum::NO),
            ("always", RestartPolicyNameEnum::ALWAYS),
            ("on-failure", RestartPolicyNameEnum::ON_FAILURE),
            ("unless-stopped", RestartPolicyNameEnum::UNLESS_STOPPED),
            ("anything-else", RestartPolicyNameEnum::UNLESS_STOPPED),
        ] {
            let mut svc = ServiceConfig::test_service("api", "api:v1");
            svc.restart = policy_str.to_string();
            let config = build_container_config("proj", &svc, 1, 0, "net", &HashMap::new(), None);
            let restart = config.host_config.unwrap().restart_policy.unwrap();
            assert_eq!(
                restart.name,
                Some(expected),
                "Failed for policy: {}",
                policy_str
            );
        }
    }

    #[test]
    fn test_build_container_config_with_command_and_entrypoint() {
        let mut svc = ServiceConfig::test_service("api", "api:v1");
        svc.command = Some(vec![
            "serve".to_string(),
            "--port".to_string(),
            "8080".to_string(),
        ]);
        svc.entrypoint = Some(vec!["/bin/sh".to_string(), "-c".to_string()]);

        let config = build_container_config("proj", &svc, 1, 0, "net", &HashMap::new(), None);
        assert_eq!(config.cmd.unwrap(), vec!["serve", "--port", "8080"]);
        assert_eq!(config.entrypoint.unwrap(), vec!["/bin/sh", "-c"]);
    }

    #[test]
    fn test_build_container_config_traefik_labels() {
        let mut svc = ServiceConfig::test_service("api", "api:v1");
        svc.routing = Some(RoutingConfig {
            rule: "Host(`api.example.com`)".to_string(),
            entrypoints: vec!["web".to_string()],
            tls: false,
        });
        svc.ports = Some(PortsConfig {
            container: 8080,
            host: None,
            host_base: None,
        });

        let config =
            build_container_config("myapp", &svc, 5, 0, "korgi-traefik", &HashMap::new(), None);
        let labels = config.labels.unwrap();
        assert_eq!(labels.get("traefik.enable").unwrap(), "true");
        assert_eq!(
            labels.get("traefik.http.routers.myapp-api.rule").unwrap(),
            "Host(`api.example.com`)"
        );
        assert_eq!(labels.get("korgi.generation").unwrap(), "5");
    }
}
