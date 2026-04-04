use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Root configuration loaded from korgi.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub project: ProjectConfig,
    #[serde(default)]
    pub registries: Vec<RegistryConfig>,
    pub hosts: Vec<HostConfig>,
    #[serde(default)]
    pub traefik: Option<TraefikConfig>,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryConfig {
    pub url: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

/// Host role: load balancer (runs Traefik) or node (runs containers).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostRole {
    /// Load balancer -- runs Traefik, faces the internet. No app containers by default.
    Lb,
    /// Node -- runs application containers.
    #[default]
    Node,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    pub name: String,
    /// Host role: "lb" (runs Traefik) or "node" (runs containers, default).
    #[serde(default)]
    pub role: HostRole,
    /// Public/external address -- used for SSH connections.
    pub address: String,
    /// Internal/private address -- used for Traefik load balancing and inter-host traffic.
    /// Falls back to `address` if not set.
    #[serde(default)]
    pub internal_address: Option<String>,
    #[serde(default = "default_ssh_user")]
    pub user: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    #[serde(default)]
    pub ssh_key: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub docker_socket: Option<String>,
}

impl HostConfig {
    pub fn is_lb(&self) -> bool {
        self.role == HostRole::Lb
    }

    pub fn is_node(&self) -> bool {
        self.role == HostRole::Node
    }

    /// Returns the address to use for SSH connections (public/external).
    pub fn ssh_address(&self) -> &str {
        &self.address
    }

    /// Returns the address to use for internal traffic (Traefik, service-to-service).
    /// Falls back to the SSH address if no internal address is configured.
    pub fn internal_addr(&self) -> &str {
        self.internal_address.as_deref().unwrap_or(&self.address)
    }

    /// Helper to build a minimal HostConfig for testing.
    #[cfg(test)]
    pub fn test_host(name: &str, address: &str) -> Self {
        Self {
            name: name.to_string(),
            role: HostRole::Node,
            address: address.to_string(),
            internal_address: None,
            user: "deploy".to_string(),
            port: 22,
            ssh_key: None,
            labels: vec![],
            docker_socket: None,
        }
    }
}

fn default_ssh_user() -> String {
    "root".to_string()
}

fn default_ssh_port() -> u16 {
    22
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraefikConfig {
    #[serde(default = "default_traefik_image")]
    pub image: String,
    /// Deprecated: use `role = "lb"` on hosts instead. If set, overrides role-based detection.
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub entrypoints: HashMap<String, String>,
    #[serde(default = "default_traefik_network")]
    pub network: String,
    #[serde(default)]
    pub acme: Option<AcmeConfig>,
}

fn default_traefik_image() -> String {
    "traefik:v3.2".to_string()
}

fn default_traefik_network() -> String {
    "korgi-traefik".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcmeConfig {
    pub email: String,
    #[serde(default = "default_acme_storage")]
    pub storage: String,
}

fn default_acme_storage() -> String {
    "/letsencrypt/acme.json".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub name: String,
    pub image: String,
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    #[serde(default)]
    pub placement_labels: Vec<String>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default = "default_restart")]
    pub restart: String,
    #[serde(default)]
    pub health: Option<HealthConfig>,
    #[serde(default)]
    pub routing: Option<RoutingConfig>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub ports: Option<PortsConfig>,
    #[serde(default)]
    pub volumes: Vec<VolumeConfig>,
    #[serde(default)]
    pub resources: Option<ResourcesConfig>,
    #[serde(default)]
    pub deploy: Option<DeployConfig>,
}

fn default_replicas() -> u32 {
    1
}

fn default_restart() -> String {
    "unless-stopped".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthConfig {
    pub path: String,
    #[serde(default = "default_health_interval")]
    pub interval: String,
    #[serde(default = "default_health_timeout")]
    pub timeout: String,
    #[serde(default = "default_health_retries")]
    pub retries: u32,
    #[serde(default)]
    pub start_period: Option<String>,
}

fn default_health_interval() -> String {
    "5s".to_string()
}

fn default_health_timeout() -> String {
    "3s".to_string()
}

fn default_health_retries() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    pub rule: String,
    #[serde(default)]
    pub entrypoints: Vec<String>,
    #[serde(default)]
    pub tls: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortsConfig {
    pub container: u16,
    #[serde(default)]
    pub host: Option<u16>,
    /// Base port for auto-allocated host port bindings across replicas.
    /// Instance 0 gets host_base, instance 1 gets host_base+1, etc.
    /// Required for cross-host load balancing via Traefik file provider.
    #[serde(default)]
    pub host_base: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeConfig {
    pub host: String,
    pub container: String,
    #[serde(default)]
    pub readonly: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourcesConfig {
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub cpus: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployConfig {
    #[serde(default = "default_drain_seconds")]
    pub drain_seconds: u64,
    #[serde(default = "default_start_delay")]
    pub start_delay: u64,
    #[serde(default = "default_rollback_keep")]
    pub rollback_keep: u32,
}

fn default_drain_seconds() -> u64 {
    30
}

fn default_start_delay() -> u64 {
    5
}

fn default_rollback_keep() -> u32 {
    2
}

impl Default for DeployConfig {
    fn default() -> Self {
        Self {
            drain_seconds: default_drain_seconds(),
            start_delay: default_start_delay(),
            rollback_keep: default_rollback_keep(),
        }
    }
}

impl ServiceConfig {
    /// Helper to build a minimal ServiceConfig for testing.
    #[cfg(test)]
    pub fn test_service(name: &str, image: &str) -> Self {
        Self {
            name: name.to_string(),
            image: image.to_string(),
            replicas: 1,
            placement_labels: vec![],
            command: None,
            entrypoint: None,
            restart: "unless-stopped".to_string(),
            health: None,
            routing: None,
            env: HashMap::new(),
            ports: None,
            volumes: vec![],
            resources: None,
            deploy: None,
        }
    }
}

impl Config {
    /// Find a service config by name.
    pub fn find_service(&self, name: &str) -> Option<&ServiceConfig> {
        self.services.iter().find(|s| s.name == name)
    }

    pub fn load(path: &PathBuf) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read config file {}: {}", path.display(), e))?;
        let config: Config = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse config file {}: {}", path.display(), e))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.project.name.is_empty() {
            anyhow::bail!("project.name cannot be empty");
        }
        if self.hosts.is_empty() {
            anyhow::bail!("at least one host must be defined");
        }
        for host in &self.hosts {
            if host.name.is_empty() {
                anyhow::bail!("host name cannot be empty");
            }
            if host.address.is_empty() {
                anyhow::bail!("host '{}' address cannot be empty", host.name);
            }
        }
        // Validate traefik config
        if let Some(traefik) = &self.traefik {
            // If hosts is explicitly set, validate references (backwards compat)
            if !traefik.hosts.is_empty() {
                let host_names: Vec<&str> = self.hosts.iter().map(|h| h.name.as_str()).collect();
                for th in &traefik.hosts {
                    if !host_names.contains(&th.as_str()) {
                        anyhow::bail!("traefik references unknown host '{}'", th);
                    }
                }
            } else {
                // No explicit hosts -- must have at least one role=lb host
                if self.lb_hosts().is_empty() {
                    anyhow::bail!(
                        "[traefik] is configured but no hosts have role = \"lb\". \
                         Add role = \"lb\" to at least one host."
                    );
                }
            }
        }
        // Validate no duplicate service names
        let mut seen_services = std::collections::HashSet::new();
        for svc in &self.services {
            if !seen_services.insert(&svc.name) {
                anyhow::bail!("duplicate service name '{}'", svc.name);
            }
        }
        // Validate service placement labels exist on at least one host
        for svc in &self.services {
            if svc.name.is_empty() {
                anyhow::bail!("service name cannot be empty");
            }
            if svc.image.is_empty() {
                anyhow::bail!("service '{}' image cannot be empty", svc.name);
            }
            if !svc.placement_labels.is_empty() {
                let has_matching_host = self.hosts.iter().any(|h| {
                    svc.placement_labels.iter().all(|pl| h.labels.contains(pl))
                });
                if !has_matching_host {
                    anyhow::bail!(
                        "service '{}' placement_labels {:?} don't match any host",
                        svc.name,
                        svc.placement_labels
                    );
                }
            }
        }
        Ok(())
    }

    /// Get hosts with role = lb.
    pub fn lb_hosts(&self) -> Vec<&HostConfig> {
        self.hosts.iter().filter(|h| h.is_lb()).collect()
    }

    /// Get hosts with role = node.
    pub fn node_hosts(&self) -> Vec<&HostConfig> {
        self.hosts.iter().filter(|h| h.is_node()).collect()
    }

    /// Get the names of hosts that should run Traefik.
    /// Uses explicit `[traefik].hosts` if set (backwards compat), otherwise role=lb hosts.
    pub fn traefik_host_names(&self) -> Vec<String> {
        if let Some(traefik) = &self.traefik
            && !traefik.hosts.is_empty()
        {
            return traefik.hosts.clone();
        }
        self.lb_hosts().iter().map(|h| h.name.clone()).collect()
    }

    /// Get the deploy config for a service, using defaults if not specified.
    pub fn deploy_config(svc: &ServiceConfig) -> DeployConfig {
        svc.deploy.clone().unwrap_or_default()
    }

    /// Find hosts matching a service's placement labels.
    pub fn matching_hosts(&self, svc: &ServiceConfig) -> Vec<&HostConfig> {
        if svc.placement_labels.is_empty() {
            return self.hosts.iter().collect();
        }
        self.hosts
            .iter()
            .filter(|h| {
                svc.placement_labels
                    .iter()
                    .all(|pl| h.labels.contains(pl))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config() -> Config {
        Config {
            project: ProjectConfig {
                name: "myapp".to_string(),
            },
            registries: vec![],
            hosts: vec![{
                let mut h = HostConfig::test_host("web1", "192.168.1.10");
                h.labels = vec!["web".to_string()];
                h
            }],
            traefik: None,
            services: vec![ServiceConfig::test_service("api", "myapp/api:latest")],
        }
    }

    // --- Config Parsing from TOML ---

    #[test]
    fn test_parse_minimal_toml() {
        let toml_str = r#"
            [project]
            name = "myapp"

            [[hosts]]
            name = "web1"
            address = "10.0.0.1"

            [[services]]
            name = "api"
            image = "api:latest"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.project.name, "myapp");
        assert_eq!(config.hosts.len(), 1);
        assert_eq!(config.hosts[0].user, "root"); // default
        assert_eq!(config.services.len(), 1);
        assert_eq!(config.services[0].replicas, 1); // default
        assert_eq!(config.services[0].restart, "unless-stopped"); // default
    }

    #[test]
    fn test_parse_full_toml() {
        let toml_str = r#"
            [project]
            name = "fullapp"

            [[registries]]
            url = "ghcr.io"
            username = "user"
            password = "pass"

            [[hosts]]
            name = "web1"
            address = "10.0.0.1"
            user = "deploy"
            ssh_key = "~/.ssh/id_ed25519"
            labels = ["web", "primary"]
            docker_socket = "/var/run/docker.sock"

            [[hosts]]
            name = "web2"
            address = "10.0.0.2"
            user = "deploy"
            labels = ["web"]

            [traefik]
            image = "traefik:v3.2"
            hosts = ["web1", "web2"]
            network = "my-network"

            [traefik.acme]
            email = "admin@example.com"

            [[services]]
            name = "api"
            image = "myapp/api:v1"
            replicas = 3
            placement_labels = ["web"]
            command = ["serve"]
            entrypoint = ["/bin/sh", "-c"]
            restart = "always"

            [services.health]
            path = "/health"
            interval = "10s"
            timeout = "5s"
            retries = 5
            start_period = "15s"

            [services.routing]
            rule = "Host(`api.example.com`)"
            entrypoints = ["websecure"]
            tls = true

            [services.env]
            DATABASE_URL = "postgres://localhost/db"

            [services.ports]
            container = 8080
            host = 9090

            [[services.volumes]]
            host = "/data"
            container = "/app/data"
            readonly = true

            [services.resources]
            memory = "512m"
            cpus = "1.5"

            [services.deploy]
            drain_seconds = 60
            start_delay = 10
            rollback_keep = 3
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.project.name, "fullapp");
        assert_eq!(config.registries.len(), 1);
        assert_eq!(config.hosts.len(), 2);
        assert_eq!(config.hosts[0].labels.len(), 2);
        assert!(config.traefik.is_some());
        let traefik = config.traefik.as_ref().unwrap();
        assert_eq!(traefik.network, "my-network");
        assert!(traefik.acme.is_some());
        let svc = &config.services[0];
        assert_eq!(svc.replicas, 3);
        assert_eq!(svc.command.as_ref().unwrap(), &vec!["serve".to_string()]);
        assert_eq!(svc.restart, "always");
        let health = svc.health.as_ref().unwrap();
        assert_eq!(health.path, "/health");
        assert_eq!(health.retries, 5);
        assert_eq!(health.start_period.as_ref().unwrap(), "15s");
        let routing = svc.routing.as_ref().unwrap();
        assert!(routing.tls);
        assert_eq!(routing.entrypoints, vec!["websecure"]);
        let ports = svc.ports.as_ref().unwrap();
        assert_eq!(ports.container, 8080);
        assert_eq!(ports.host, Some(9090));
        assert_eq!(svc.volumes.len(), 1);
        assert!(svc.volumes[0].readonly);
        let resources = svc.resources.as_ref().unwrap();
        assert_eq!(resources.memory.as_ref().unwrap(), "512m");
        let deploy = svc.deploy.as_ref().unwrap();
        assert_eq!(deploy.drain_seconds, 60);
        assert_eq!(deploy.rollback_keep, 3);
    }

    #[test]
    fn test_parse_defaults() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
            [[services]]
            name = "web"
            image = "web:latest"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let svc = &config.services[0];
        assert_eq!(svc.replicas, 1);
        assert_eq!(svc.restart, "unless-stopped");
        assert!(svc.health.is_none());
        assert!(svc.routing.is_none());
        assert!(svc.deploy.is_none());
        assert!(svc.env.is_empty());
        assert!(svc.volumes.is_empty());
    }

    #[test]
    fn test_traefik_defaults() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
            [traefik]
            hosts = ["h1"]
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let traefik = config.traefik.unwrap();
        assert_eq!(traefik.image, "traefik:v3.2");
        assert_eq!(traefik.network, "korgi-traefik");
        assert!(traefik.acme.is_none());
    }

    #[test]
    fn test_health_defaults() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
            [[services]]
            name = "web"
            image = "web:latest"
            [services.health]
            path = "/ready"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let health = config.services[0].health.as_ref().unwrap();
        assert_eq!(health.interval, "5s");
        assert_eq!(health.timeout, "3s");
        assert_eq!(health.retries, 3);
        assert!(health.start_period.is_none());
    }

    // --- Validation ---

    #[test]
    fn test_validate_empty_project_name() {
        let mut config = minimal_config();
        config.project.name = "".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_no_hosts() {
        let mut config = minimal_config();
        config.hosts.clear();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_empty_host_name() {
        let mut config = minimal_config();
        config.hosts[0].name = "".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_empty_host_address() {
        let mut config = minimal_config();
        config.hosts[0].address = "".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_empty_service_name() {
        let mut config = minimal_config();
        config.services[0].name = "".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_empty_service_image() {
        let mut config = minimal_config();
        config.services[0].image = "".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_traefik_references_unknown_host() {
        let mut config = minimal_config();
        config.traefik = Some(TraefikConfig {
            image: "traefik:v3.2".to_string(),
            hosts: vec!["nonexistent".to_string()],
            entrypoints: HashMap::new(),
            network: "korgi-traefik".to_string(),
            acme: None,
        });
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("unknown host"));
    }

    #[test]
    fn test_validate_traefik_references_valid_host() {
        let mut config = minimal_config();
        config.traefik = Some(TraefikConfig {
            image: "traefik:v3.2".to_string(),
            hosts: vec!["web1".to_string()],
            entrypoints: HashMap::new(),
            network: "korgi-traefik".to_string(),
            acme: None,
        });
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_placement_labels_no_match() {
        let mut config = minimal_config();
        config.services[0].placement_labels = vec!["gpu".to_string()];
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("placement_labels"));
        assert!(err.to_string().contains("gpu"));
    }

    #[test]
    fn test_validate_placement_labels_match() {
        let mut config = minimal_config();
        config.services[0].placement_labels = vec!["web".to_string()];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_placement_labels_partial_match() {
        // Service requires ["web", "gpu"] but host only has ["web"]
        let mut config = minimal_config();
        config.services[0].placement_labels = vec!["web".to_string(), "gpu".to_string()];
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_multiple_hosts_one_matches() {
        let mut config = minimal_config();
        let mut gpu = HostConfig::test_host("gpu1", "10.0.0.2");
        gpu.labels = vec!["gpu".to_string()];
        config.hosts.push(gpu);
        config.services[0].placement_labels = vec!["gpu".to_string()];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_no_services_ok() {
        let mut config = minimal_config();
        config.services.clear();
        assert!(config.validate().is_ok());
    }

    // --- matching_hosts ---

    #[test]
    fn test_matching_hosts_no_labels_matches_all() {
        let config = minimal_config();
        let svc = ServiceConfig::test_service("test", "img:latest");
        let hosts = config.matching_hosts(&svc);
        assert_eq!(hosts.len(), 1);
    }

    #[test]
    fn test_matching_hosts_with_labels() {
        let mut config = minimal_config();
        let mut gpu = HostConfig::test_host("gpu1", "10.0.0.2");
        gpu.labels = vec!["gpu".to_string()];
        config.hosts.push(gpu);
        let mut svc = ServiceConfig::test_service("test", "img:latest");
        svc.placement_labels = vec!["gpu".to_string()];
        let hosts = config.matching_hosts(&svc);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "gpu1");
    }

    #[test]
    fn test_matching_hosts_multiple_labels_all_required() {
        let mut config = minimal_config();
        config.hosts[0].labels = vec!["web".to_string(), "primary".to_string()];
        let mut web2 = HostConfig::test_host("web2", "10.0.0.2");
        web2.labels = vec!["web".to_string()];
        config.hosts.push(web2);
        let mut svc = ServiceConfig::test_service("test", "img:latest");
        svc.placement_labels = vec!["web".to_string(), "primary".to_string()];
        let hosts = config.matching_hosts(&svc);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "web1");
    }

    // --- deploy_config ---

    #[test]
    fn test_deploy_config_defaults() {
        let svc = ServiceConfig::test_service("test", "img:latest");
        let deploy = Config::deploy_config(&svc);
        assert_eq!(deploy.drain_seconds, 30);
        assert_eq!(deploy.start_delay, 5);
        assert_eq!(deploy.rollback_keep, 2);
    }

    #[test]
    fn test_deploy_config_custom() {
        let mut svc = ServiceConfig::test_service("test", "img:latest");
        svc.deploy = Some(DeployConfig {
            drain_seconds: 120,
            start_delay: 15,
            rollback_keep: 5,
        });
        let deploy = Config::deploy_config(&svc);
        assert_eq!(deploy.drain_seconds, 120);
        assert_eq!(deploy.start_delay, 15);
        assert_eq!(deploy.rollback_keep, 5);
    }

    // --- find_service ---

    #[test]
    fn test_find_service() {
        let config = minimal_config();
        assert!(config.find_service("api").is_some());
        assert!(config.find_service("nonexistent").is_none());
    }

    // --- Serialization round-trip ---

    #[test]
    fn test_config_roundtrip() {
        let config = minimal_config();
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.project.name, config.project.name);
        assert_eq!(parsed.hosts.len(), config.hosts.len());
        assert_eq!(parsed.services.len(), config.services.len());
    }

    // --- Multiple services ---

    #[test]
    fn test_multiple_services() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
            [[services]]
            name = "api"
            image = "api:latest"
            replicas = 3
            [[services]]
            name = "worker"
            image = "worker:latest"
            replicas = 2
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.services.len(), 2);
        assert_eq!(config.services[0].name, "api");
        assert_eq!(config.services[0].replicas, 3);
        assert_eq!(config.services[1].name, "worker");
        assert_eq!(config.services[1].replicas, 2);
    }

    // --- Duplicate service names ---

    #[test]
    fn test_validate_duplicate_service_names() {
        let mut config = minimal_config();
        config.services.push(ServiceConfig::test_service("api", "other:latest"));
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("duplicate service name"));
    }

    // --- SSH port ---

    #[test]
    fn test_host_default_port() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.hosts[0].port, 22);
    }

    #[test]
    fn test_host_custom_port() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
            port = 2222
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.hosts[0].port, 2222);
    }

    // --- Internal address ---

    #[test]
    fn test_host_no_internal_address() {
        let h = HostConfig::test_host("web1", "1.2.3.4");
        assert_eq!(h.ssh_address(), "1.2.3.4");
        assert_eq!(h.internal_addr(), "1.2.3.4"); // falls back to address
    }

    #[test]
    fn test_host_with_internal_address() {
        let mut h = HostConfig::test_host("web1", "203.0.113.10");
        h.internal_address = Some("10.0.0.1".to_string());
        assert_eq!(h.ssh_address(), "203.0.113.10"); // public
        assert_eq!(h.internal_addr(), "10.0.0.1"); // private
    }

    #[test]
    fn test_host_internal_address_from_toml() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "web1"
            address = "203.0.113.10"
            internal_address = "10.0.0.1"
            port = 2222
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let host = &config.hosts[0];
        assert_eq!(host.ssh_address(), "203.0.113.10");
        assert_eq!(host.internal_addr(), "10.0.0.1");
        assert_eq!(host.port, 2222);
    }

    // --- Host roles ---

    #[test]
    fn test_role_default_is_node() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.hosts[0].role, HostRole::Node);
        assert!(config.hosts[0].is_node());
        assert!(!config.hosts[0].is_lb());
    }

    #[test]
    fn test_role_lb() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "lb"
            role = "lb"
            address = "1.2.3.4"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.hosts[0].role, HostRole::Lb);
        assert!(config.hosts[0].is_lb());
    }

    #[test]
    fn test_lb_hosts_and_node_hosts() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "lb"
            role = "lb"
            address = "1.2.3.4"
            [[hosts]]
            name = "w1"
            address = "5.6.7.8"
            [[hosts]]
            name = "w2"
            role = "node"
            address = "9.10.11.12"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.lb_hosts().len(), 1);
        assert_eq!(config.lb_hosts()[0].name, "lb");
        assert_eq!(config.node_hosts().len(), 2);
    }

    #[test]
    fn test_traefik_host_names_from_role() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "lb1"
            role = "lb"
            address = "1.2.3.4"
            [[hosts]]
            name = "lb2"
            role = "lb"
            address = "5.6.7.8"
            [[hosts]]
            name = "w1"
            address = "9.10.11.12"
            [traefik]
            entrypoints = { web = ":80" }
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let names = config.traefik_host_names();
        assert_eq!(names, vec!["lb1", "lb2"]);
    }

    #[test]
    fn test_traefik_host_names_explicit_overrides_role() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "lb1"
            role = "lb"
            address = "1.2.3.4"
            [[hosts]]
            name = "w1"
            address = "5.6.7.8"
            [traefik]
            hosts = ["w1"]
            entrypoints = { web = ":80" }
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        // Explicit hosts overrides role-based detection
        let names = config.traefik_host_names();
        assert_eq!(names, vec!["w1"]);
    }

    #[test]
    fn test_validate_traefik_no_lb_hosts_fails() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "w1"
            address = "1.2.3.4"
            [traefik]
            entrypoints = { web = ":80" }
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("role = \"lb\""));
    }

    #[test]
    fn test_validate_traefik_with_lb_hosts_ok() {
        let toml_str = r#"
            [project]
            name = "app"
            [[hosts]]
            name = "lb"
            role = "lb"
            address = "1.2.3.4"
            [traefik]
            entrypoints = { web = ":80" }
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.validate().is_ok());
    }
}
