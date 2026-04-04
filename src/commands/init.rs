use anyhow::Result;
use std::path::Path;

use crate::cli::output;

const TEMPLATE: &str = r#"[project]
name = "myapp"

# [[registries]]
# url = "ghcr.io"
# username = "${GHCR_USER}"
# password = "${GHCR_TOKEN}"

[[hosts]]
name = "web1"
address = "192.168.1.10"          # public IP (SSH connects here)
# internal_address = "10.0.0.1"  # private IP (Traefik/internal traffic)
# port = 22                      # SSH port (default: 22)
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["web"]

# [[hosts]]
# name = "web2"
# address = "192.168.1.11"
# internal_address = "10.0.0.2"
# user = "deploy"
# ssh_key = "~/.ssh/id_ed25519"
# labels = ["web"]

[traefik]
image = "traefik:v3.2"
hosts = ["web1"]
entrypoints = { web = ":80", websecure = ":443" }
network = "korgi-traefik"

# [traefik.acme]
# email = "admin@example.com"
# storage = "/letsencrypt/acme.json"

[[services]]
name = "web"
image = "myapp/web:latest"
replicas = 2
placement_labels = ["web"]

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

[services.env]
# DATABASE_URL = "${DATABASE_URL}"

[services.deploy]
drain_seconds = 30
start_delay = 5
rollback_keep = 2
"#;

pub fn run(config_path: &Path) -> Result<()> {
    if config_path.exists() {
        anyhow::bail!(
            "Config file already exists: {}",
            config_path.display()
        );
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
