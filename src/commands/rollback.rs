use anyhow::Result;
use std::collections::HashMap;

use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::rollback::rollback_service;

pub async fn run(
    config: &Config,
    service: &str,
    auto_yes: bool,
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    use crate::cli::output;
    if !output::confirm(&format!("Roll back '{}'?", service), auto_yes) {
        output::info("Cancelled");
        return Ok(());
    }
    // Health checks and Traefik syncs are ordered inside rollback_service.
    rollback_service(config, service, docker_hosts).await
}
