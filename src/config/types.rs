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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    pub name: String,
    pub address: String,
    #[serde(default = "default_ssh_user")]
    pub user: String,
    #[serde(default)]
    pub ssh_key: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub docker_socket: Option<String>,
}

fn default_ssh_user() -> String {
    "root".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraefikConfig {
    #[serde(default = "default_traefik_image")]
    pub image: String,
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

impl Config {
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
        // Validate traefik host references
        if let Some(traefik) = &self.traefik {
            let host_names: Vec<&str> = self.hosts.iter().map(|h| h.name.as_str()).collect();
            for th in &traefik.hosts {
                if !host_names.contains(&th.as_str()) {
                    anyhow::bail!("traefik references unknown host '{}'", th);
                }
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
