use anyhow::Result;
use std::collections::HashMap;

use crate::commands::sync_config;
use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::scale::scale_service;

pub async fn run(
    config: &Config,
    service: &str,
    count: u32,
    auto_yes: bool,
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    use crate::cli::output;
    if !output::confirm(
        &format!("Scale '{}' to {} replicas?", service, count),
        auto_yes,
    ) {
        output::info("Cancelled");
        return Ok(());
    }
    scale_service(config, service, count, docker_hosts).await?;
    sync_config::sync_traefik_config(config, docker_hosts).await?;
    Ok(())
}
