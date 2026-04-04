use anyhow::{Context, Result};
use std::collections::HashMap;
use tracing::debug;

use crate::cli::output;
use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::state::LiveState;

/// Roll back a service to the most recent stopped generation.
pub async fn rollback_service(
    config: &Config,
    service: &str,
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    let state = LiveState::query(docker_hosts, &config.project.name).await?;
    let deploy_cfg = Config::deploy_config(
        config
            .services
            .iter()
            .find(|s| s.name == service)
            .context(format!("Service '{}' not found in config", service))?,
    );

    let current_gen = state
        .current_generation(service)
        .context(format!("No running containers for service '{}'", service))?;

    let rollback_gen = state
        .rollback_generation(service)
        .context(format!(
            "No previous generation found to roll back to for '{}'",
            service
        ))?;

    output::info(&format!(
        "Rolling back '{}' from generation {} to {}",
        service, current_gen, rollback_gen
    ));

    let rollback_containers = state.generation_containers(service, rollback_gen);
    if rollback_containers.is_empty() {
        anyhow::bail!("No containers found for generation {}", rollback_gen);
    }

    // Verify images exist and start rollback containers
    let pb = output::spinner("Starting rollback containers...");
    for container in &rollback_containers {
        let docker = docker_hosts
            .get(&container.host_name)
            .context(format!("No connection to host {}", container.host_name))?;

        // Check if image still exists
        if !docker.image_exists(&container.image).await? {
            output::info(&format!(
                "Image {} missing on {}, pulling...",
                container.image, container.host_name
            ));
            docker.pull_image(&container.image, None).await?;
        }

        // Start the stopped container
        docker.start_container(&container.id).await.with_context(|| {
            format!(
                "Failed to start rollback container {} on {}",
                container.name, container.host_name
            )
        })?;
        debug!("Started rollback container {}", container.name);
    }
    pb.finish_and_clear();
    output::success(&format!(
        "Started {} rollback containers",
        rollback_containers.len()
    ));

    // Stop current generation
    let current_containers = state.generation_containers(service, current_gen);
    let running_current: Vec<_> = current_containers
        .iter()
        .filter(|c| c.state == "running")
        .collect();

    if !running_current.is_empty() {
        let pb = output::spinner("Stopping current generation...");
        for container in &running_current {
            if let Some(docker) = docker_hosts.get(&container.host_name) {
                docker
                    .stop_container(&container.id, deploy_cfg.drain_seconds as i64)
                    .await
                    .ok();
            }
        }
        pb.finish_and_clear();
        output::success(&format!(
            "Stopped {} containers from generation {}",
            running_current.len(),
            current_gen
        ));
    }

    output::success(&format!(
        "Rollback complete: {} now running generation {}",
        service, rollback_gen
    ));

    Ok(())
}
