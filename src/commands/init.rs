use anyhow::Result;
use std::path::Path;

use crate::cli::output;

const TEMPLATE: &str = r#"[project]
name = "myapp"

# [[registries]]
# url = "ghcr.io"
# username = "${GHCR_USER}"
# password = "${GHCR_TOKEN}"

# --- Load balancer (runs Traefik, faces the internet) ---
[[hosts]]
name = "lb"
role = "lb"                        # runs Traefik -- no app containers
address = "203.0.113.1"            # public IP (SSH connects here)
internal_address = "10.0.0.1"      # private IP (Traefik routes via this)
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"

# --- Worker nodes (run containers, internal only) ---
[[hosts]]
name = "worker-1"
role = "node"                      # default -- runs app containers
address = "10.0.0.10"
internal_address = "10.0.0.10"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["app"]

[[hosts]]
name = "worker-2"
address = "10.0.0.11"
internal_address = "10.0.0.11"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["app"]

[traefik]
image = "traefik:v3.2"
entrypoints = { web = ":80", websecure = ":443" }
network = "korgi-traefik"

# [traefik.acme]
# email = "admin@example.com"
# storage = "/letsencrypt/acme.json"

[[services]]
name = "web"
image = "myapp/web:latest"
replicas = 4
placement_labels = ["app"]         # Only placed on worker hosts

[services.health]
path = "/health"
interval = "5s"
timeout = "3s"
retries = 3

[services.routing]
rule = "Host(`myapp.example.com`)"
entrypoints = ["web"]

[services.ports]
container = 8080
host_base = 9001                   # Workers expose 9001, 9002, ... for Traefik to reach

[services.env]
# DATABASE_URL = "${DATABASE_URL}"

[services.deploy]
drain_seconds = 30
start_delay = 5
rollback_keep = 2
"#;

pub fn run(config_path: &Path) -> Result<()> {
    if config_path.exists() {
        anyhow::bail!("Config file already exists: {}", config_path.display());
    }

    std::fs::write(config_path, TEMPLATE)?;
    output::success(&format!("Created {}", config_path.display()));
    output::info("Edit the file to configure your hosts and services, then run 'korgi check'");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_init_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("korgi.toml");
        run(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_init_template_is_valid_toml() {
        // The template should parse as valid TOML
        let _: toml::Value = toml::from_str(TEMPLATE).expect("Template is not valid TOML");
    }

    #[test]
    fn test_init_template_parses_as_config() {
        // The template should parse as a valid Config (after removing comments)
        use crate::config::types::Config;
        let config: Config = toml::from_str(TEMPLATE).expect("Template doesn't parse as Config");
        assert_eq!(config.project.name, "myapp");
        assert!(!config.hosts.is_empty());
        assert!(!config.services.is_empty());
    }

    #[test]
    fn test_init_fails_if_exists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("korgi.toml");
        std::fs::write(&path, "existing content").unwrap();
        let result = run(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
        // Original file should be untouched
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "existing content");
    }

    #[test]
    fn test_init_template_validates() {
        use crate::config::types::Config;
        let config: Config = toml::from_str(TEMPLATE).unwrap();
        config.validate().expect("Template config should validate");
    }
}
