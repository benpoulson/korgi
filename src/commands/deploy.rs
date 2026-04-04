use anyhow::Result;
use std::collections::HashMap;

use crate::cli::output;
use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::deploy::deploy_service;

pub async fn run(
    config: &Config,
    service_filter: Option<&str>,
    image_override: Option<&str>,
    dry_run: bool,
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    let services: Vec<_> = if let Some(name) = service_filter {
        let svc = config
            .services
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| anyhow::anyhow!("Service '{}' not found in config", name))?;
        vec![svc]
    } else {
        config.services.iter().collect()
    };

    if dry_run {
        output::info("Dry run mode -- no changes will be made");
    }

    for svc in &services {
        deploy_service(config, svc, image_override, docker_hosts, dry_run).await?;
    }

    if services.len() > 1 {
        output::success(&format!("All {} services deployed", services.len()));
    }

    Ok(())
}
