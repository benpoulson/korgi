use anyhow::Result;
use std::collections::HashMap;

use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::rollback::rollback_service;

pub async fn run(
    config: &Config,
    service: &str,
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    rollback_service(config, service, docker_hosts).await
}
