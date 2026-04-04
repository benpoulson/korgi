use anyhow::Result;
use std::collections::HashMap;

use crate::commands::sync_config;
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
    rollback_service(config, service, docker_hosts).await?;
    sync_config::sync_traefik_config(config, docker_hosts).await?;
    Ok(())
}
