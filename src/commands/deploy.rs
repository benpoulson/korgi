use anyhow::Result;
use std::collections::HashMap;

use crate::cli::output;
use crate::commands::sync_config;
use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::deploy::deploy_service;

pub async fn run(
    config: &Config,
    service_filter: Option<&str>,
    image_override: Option<&str>,
    dry_run: bool,
    auto_yes: bool,
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

    if !dry_run {
        let svc_names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();
        let host_count = config.node_hosts().len();
        let msg = format!("Deploy {} to {} hosts?", svc_names.join(", "), host_count,);
        if !output::confirm(&msg, auto_yes) {
            output::info("Cancelled");
            return Ok(());
        }
    }

    for svc in &services {
        deploy_service(config, svc, image_override, docker_hosts, dry_run).await?;
    }

    if services.len() > 1 {
        output::success(&format!("All {} services deployed", services.len()));
    }

    // Sync Traefik routing config with new container topology
    if !dry_run {
        sync_config::sync_traefik_config(config, docker_hosts).await?;
    }

    Ok(())
}
