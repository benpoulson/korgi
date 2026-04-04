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
address = "192.168.1.10"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["web"]

# [[hosts]]
# name = "web2"
# address = "192.168.1.11"
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
