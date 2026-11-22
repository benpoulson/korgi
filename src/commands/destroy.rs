use anyhow::Result;
use std::collections::{HashMap, HashSet};

use crate::cli::output;
use crate::commands::sync_config;
use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::state::LiveState;
use crate::orchestrator::verify::{self, ContainerRef};

pub async fn run(
    config: &Config,
    service_filter: Option<&str>,
    auto_yes: bool,
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    let state = LiveState::query(docker_hosts, &config.project.name).await?;

    let containers: Vec<_> = if let Some(svc) = service_filter {
        state.service_containers(svc).into_iter().collect()
    } else {
        state.containers.iter().collect()
    };

    if containers.is_empty() {
        output::info("No containers to destroy");
        return Ok(());
    }

    let desc = service_filter
        .map(|s| format!("service '{}'", s))
        .unwrap_or("all services".to_string());

    if !output::confirm(
        &format!("Destroy {} containers for {}?", containers.len(), desc),
        auto_yes,
    ) {
        output::info("Cancelled");
        return Ok(());
    }

    // Take the containers out of Traefik before stopping them, so no request is
    // routed to a backend that is going away. Destroy proceeds even if this fails
    // (e.g. Traefik itself is gone), but says so.
    let ids: HashSet<String> = containers.iter().map(|c| c.id.clone()).collect();
    if let Err(e) = sync_config::sync_traefik_config_excluding(config, docker_hosts, &ids).await {
        output::warn(&format!(
            "Could not update Traefik before destroying ({:#}); continuing",
            e
        ));
    }

    let refs: Vec<(ContainerRef, bool)> = containers
        .iter()
        .map(|c| (ContainerRef::from(*c), c.is_running()))
        .collect();
    let pb = output::spinner(&format!("Destroying {} containers...", refs.len()));
    let results = futures::future::join_all(refs.iter().map(|(c, running)| async move {
        let docker = docker_hosts
            .get(&c.host_name)
            .ok_or_else(|| anyhow::anyhow!("No Docker connection for host {}", c.host_name))?;
        if *running {
            // Graceful stop first; a failure here still falls through to the
            // forced removal below.
            if let Err(e) = docker.stop_container(&c.id, 10).await {
                output::warn(&format!("Graceful stop of {} failed: {:#}", c.name, e));
            }
        }
        docker
            .remove_container(&c.id, true)
            .await
            .map_err(|e| e.context(format!("Failed to remove {} on {}", c.name, c.host_name)))
    }))
    .await;
    pb.finish_and_clear();
    let destroyed = verify::collect_errors(results);

    // Sync Traefik config to remove destroyed backends (always attempted)
    let synced = sync_config::sync_traefik_config(config, docker_hosts).await;
    destroyed?;
    output::success(&format!("Destroyed {} containers", refs.len()));
    synced?;

    Ok(())
}
