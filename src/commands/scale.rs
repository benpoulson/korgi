use anyhow::Result;
use std::collections::HashMap;

use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::scale::scale_service;

pub async fn run(
    config: &Config,
    service: &str,
    count: u32,
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    scale_service(config, service, count, docker_hosts).await
}
