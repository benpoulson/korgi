use anyhow::{Context, Result};
use std::collections::HashMap;
use tracing::debug;

use crate::cli::output;
use crate::config::interpolate;
use crate::config::types::Config;
use crate::docker::containers;
use crate::docker::host::DockerHost;
use crate::docker::labels;
use crate::orchestrator::placement;
use crate::orchestrator::state::LiveState;

/// Scale a service to a target replica count.
pub async fn scale_service(
    config: &Config,
    service: &str,
    target_count: u32,
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    let svc = config
        .services
        .iter()
        .find(|s| s.name == service)
        .context(format!("Service '{}' not found in config", service))?;

    let traefik_network = config
        .traefik
        .as_ref()
        .map(|t| t.network.as_str())
        .unwrap_or("korgi-default");

    let state = LiveState::query(docker_hosts, &config.project.name).await?;
    let current_gen = state
        .current_generation(service)
        .context(format!("No running containers for service '{}'", service))?;

    let running = state
        .generation_containers(service, current_gen)
        .into_iter()
        .filter(|c| c.state == "running")
        .collect::<Vec<_>>();

    let current_count = running.len() as u32;

    if current_count == target_count {
        output::info(&format!(
            "Service '{}' already has {} replicas",
            service, target_count
        ));
        return Ok(());
    }

    output::info(&format!(
        "Scaling '{}' from {} to {} replicas",
        service, current_count, target_count
    ));

    if target_count > current_count {
        // Scale UP: add new containers
        let add_count = target_count - current_count;
        let matching_hosts = config.matching_hosts(svc);
        let all_placements = placement::place_replicas(
            &matching_hosts.iter().map(|h| *h).collect::<Vec<_>>(),
            target_count,
        );

        // We need to create containers for instances that don't exist yet
        let existing_instances: Vec<u32> = running.iter().map(|c| c.instance).collect();

        let sys_env = interpolate::system_env();
        let resolved_env = interpolate::interpolate_env(&svc.env, &sys_env)?;

        let pb = output::spinner(&format!("Starting {} new containers...", add_count));
        for (host, instance) in &all_placements {
            if existing_instances.contains(instance) {
                continue;
            }
            let docker = docker_hosts
                .get(&host.name)
                .context(format!("No connection to host {}", host.name))?;

            docker.ensure_network(traefik_network).await?;

            let name = labels::container_name(
                &config.project.name,
                service,
                current_gen,
                *instance,
            );

            let container_config = containers::build_container_config(
                &config.project.name,
                svc,
                current_gen,
                *instance,
                traefik_network,
                &resolved_env,
            );

            let id = docker.create_container(&name, container_config).await?;
            docker.start_container(&id).await?;
            debug!("Started {} on {}", name, host.name);
        }
        pb.finish_and_clear();
        output::success(&format!("Added {} containers", add_count));
    } else {
        // Scale DOWN: remove excess containers (highest instance numbers first)
        let remove_count = current_count - target_count;
        let mut to_remove = running.clone();
        to_remove.sort_by(|a, b| b.instance.cmp(&a.instance));
        let to_remove = &to_remove[..remove_count as usize];

        let deploy_cfg = Config::deploy_config(svc);
        let pb = output::spinner(&format!("Stopping {} containers...", remove_count));
        for container in to_remove {
            if let Some(docker) = docker_hosts.get(&container.host_name) {
                docker
                    .stop_container(&container.id, deploy_cfg.drain_seconds as i64)
                    .await
                    .ok();
                docker.remove_container(&container.id, false).await.ok();
            }
        }
        pb.finish_and_clear();
        output::success(&format!("Removed {} containers", remove_count));
    }

    output::success(&format!(
        "Service '{}' scaled to {} replicas",
        service, target_count
    ));

    Ok(())
}
