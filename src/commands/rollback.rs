use anyhow::Result;
use std::collections::HashMap;

use crate::commands::sync_config;
use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::rollback::rollback_service;

pub async fn run(
    config: &Config,
    service: &str,
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    rollback_service(config, service, docker_hosts).await?;
    sync_config::sync_traefik_config(config, docker_hosts).await?;
    Ok(())
}
