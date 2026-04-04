use anyhow::{Context, Result};
use std::collections::HashMap;
use tracing::{debug, info};

use crate::cli::output;
use crate::config::types::{Config, ServiceConfig};
use crate::config::interpolate;
use crate::docker::containers::{self, KorgiContainer};
use crate::docker::host::DockerHost;
use crate::docker::labels;
use crate::docker::registry;
use crate::health;
use crate::orchestrator::placement;
use crate::orchestrator::state::LiveState;

/// Execute the zero-downtime deployment pipeline for a service.
pub async fn deploy_service(
    config: &Config,
    svc: &ServiceConfig,
    image_override: Option<&str>,
    docker_hosts: &HashMap<String, DockerHost>,
    dry_run: bool,
) -> Result<()> {
    let image = image_override.unwrap_or(&svc.image);
    let deploy_cfg = Config::deploy_config(svc);
    let traefik_network = config
        .traefik
        .as_ref()
        .map(|t| t.network.as_str())
        .unwrap_or("korgi-default");

    // Phase 1: PREPARE
    output::info(&format!(
        "Deploying service '{}' with image '{}'",
        svc.name, image
    ));

    let state = LiveState::query(docker_hosts, &config.project.name).await?;
    let generation = state.next_generation(&svc.name);
    let matching_hosts = config.matching_hosts(svc);

    if matching_hosts.is_empty() {
        anyhow::bail!("No hosts match placement labels for service '{}'", svc.name);
    }

    let placements = placement::place_replicas(
        &matching_hosts.iter().map(|h| *h).collect::<Vec<_>>(),
        svc.replicas,
    );

    info!(
        "Generation {} → {} replicas across {} hosts",
        generation,
        svc.replicas,
        matching_hosts.len()
    );

    if dry_run {
        output::info("Dry run — would deploy:");
        for (host, instance) in &placements {
            let name = labels::container_name(
                &config.project.name,
                &svc.name,
                generation,
                *instance,
            );
            output::info(&format!("  {} on {}", name, host.name));
        }
        return Ok(());
    }

    // Phase 2: PULL (parallel across unique hosts)
    let pb = output::spinner("Pulling image on target hosts...");
    let unique_hosts: Vec<&str> = placements
        .iter()
        .map(|(h, _)| h.name.as_str())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let auth = registry::credentials_for_image(image, &config.registries);

    for host_name in &unique_hosts {
        let docker = docker_hosts
            .get(*host_name)
            .context(format!("No Docker connection for host {}", host_name))?;
        let image = image.to_string();
        let auth = auth.clone();
        // We can't move docker into the task since we don't own it, so we'll pull sequentially per host
        // In a real impl we'd use Arc or similar
        docker.pull_image(&image, auth).await.with_context(|| {
            format!("Failed to pull {} on {}", image, host_name)
        })?;
    }
    pb.finish_and_clear();
    output::success(&format!("Image pulled on {} hosts", unique_hosts.len()));

    // Phase 3: ENSURE NETWORK + START GREEN
    let pb = output::spinner("Starting new containers...");

    // Ensure network exists on all target hosts
    for host_name in &unique_hosts {
        let docker = docker_hosts.get(*host_name).unwrap();
        docker.ensure_network(traefik_network).await?;
    }

    // Resolve environment variables
    let sys_env = interpolate::system_env();
    let resolved_env = interpolate::interpolate_env(&svc.env, &sys_env)?;

    // Build a modified service config with the potentially overridden image
    let mut svc_for_deploy = svc.clone();
    svc_for_deploy.image = image.to_string();

    let mut new_container_ids: Vec<(String, String)> = Vec::new(); // (host_name, container_id)

    for (host, instance) in &placements {
        let docker = docker_hosts.get(&host.name).unwrap();
        let container_name = labels::container_name(
            &config.project.name,
            &svc.name,
            generation,
            *instance,
        );

        let container_config = containers::build_container_config(
            &config.project.name,
            &svc_for_deploy,
            generation,
            *instance,
            traefik_network,
            &resolved_env,
        );

        let id = docker
            .create_container(&container_name, container_config)
            .await?;
        docker.start_container(&id).await?;
        new_container_ids.push((host.name.clone(), id));
        debug!("Started {} on {}", container_name, host.name);
    }
    pb.finish_and_clear();
    output::success(&format!(
        "Started {} new containers (generation {})",
        new_container_ids.len(),
        generation
    ));

    // Phase 4: HEALTH CHECK
    if svc.health.is_some() {
        let pb = output::spinner("Waiting for health checks...");
        let timeout = std::time::Duration::from_secs(deploy_cfg.drain_seconds * 2);

        for (host_name, container_id) in &new_container_ids {
            let docker = docker_hosts.get(host_name).unwrap();
            match health::wait_healthy(docker, container_id, timeout).await {
                Ok(()) => {
                    debug!("Container {} healthy on {}", container_id, host_name);
                }
                Err(e) => {
                    pb.finish_and_clear();
                    output::error(&format!("Health check failed: {}", e));
                    // Cleanup: stop and remove all green containers
                    output::info("Rolling back: removing new containers...");
                    for (hn, cid) in &new_container_ids {
                        let d = docker_hosts.get(hn).unwrap();
                        d.stop_container(cid, 5).await.ok();
                        d.remove_container(cid, true).await.ok();
                    }
                    anyhow::bail!("Deploy aborted: health check failed for {}", svc.name);
                }
            }
        }
        pb.finish_and_clear();
        output::success("All containers healthy");
    } else {
        // No health check — wait for start_delay
        let delay = deploy_cfg.start_delay;
        let pb = output::spinner(&format!("Waiting {}s for containers to start...", delay));
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        pb.finish_and_clear();
        output::success("Start delay elapsed");
    }

    // Phase 5: DRAIN OLD
    let old_generation = generation.checked_sub(1);
    if let Some(old_gen) = old_generation {
        let old_containers: Vec<&KorgiContainer> = state.generation_containers(&svc.name, old_gen)
            .into_iter()
            .filter(|c| c.state == "running")
            .collect();

        if !old_containers.is_empty() {
            let pb = output::spinner(&format!(
                "Draining {} old containers ({}s timeout)...",
                old_containers.len(),
                deploy_cfg.drain_seconds
            ));
            for container in &old_containers {
                if let Some(docker) = docker_hosts.get(&container.host_name) {
                    docker
                        .stop_container(&container.id, deploy_cfg.drain_seconds as i64)
                        .await
                        .ok();
                }
            }
            pb.finish_and_clear();
            output::success(&format!("Drained {} old containers", old_containers.len()));
        }
    }

    // Phase 6: CLEANUP old generations
    let keep_gens = deploy_cfg.rollback_keep;
    if generation > keep_gens as u64 + 1 {
        let cutoff = generation - keep_gens as u64 - 1;
        let to_remove: Vec<&KorgiContainer> = state
            .service_containers(&svc.name)
            .into_iter()
            .filter(|c| c.generation <= cutoff && c.state != "running")
            .collect();

        if !to_remove.is_empty() {
            debug!("Cleaning up {} old containers", to_remove.len());
            for container in &to_remove {
                if let Some(docker) = docker_hosts.get(&container.host_name) {
                    docker.remove_container(&container.id, true).await.ok();
                }
            }
            output::info(&format!("Cleaned up {} old containers", to_remove.len()));
        }
    }

    output::success(&format!(
        "Deploy complete: {} generation {} ({} replicas)",
        svc.name, generation, svc.replicas
    ));

    Ok(())
}
