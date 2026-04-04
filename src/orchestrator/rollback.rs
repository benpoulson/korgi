use anyhow::{Context, Result};
use std::collections::HashMap;
use tracing::debug;

use crate::cli::output;
use crate::config::types::Config;
use crate::docker::traits::DockerHostApi;
use crate::orchestrator::state::LiveState;

/// Roll back a service to the most recent stopped generation.
pub async fn rollback_service<D: DockerHostApi>(
    config: &Config,
    service: &str,
    docker_hosts: &HashMap<String, D>,
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

    let rollback_gen = state.rollback_generation(service).context(format!(
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
        docker
            .start_container(&container.id)
            .await
            .with_context(|| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::*;
    use crate::docker::mock::tests::*;
    use bollard::models::ContainerSummaryStateEnum;

    fn test_config() -> Config {
        Config {
            project: ProjectConfig {
                name: "myapp".to_string(),
            },
            registries: vec![],
            hosts: vec![{
                let mut h = HostConfig::test_host("web1", "10.0.0.1");
                h.labels = vec!["web".to_string()];
                h
            }],
            traefik: None,
            services: vec![{
                let mut svc = ServiceConfig::test_service("api", "myapp/api:v1");
                svc.placement_labels = vec!["web".to_string()];
                svc
            }],
        }
    }

    #[tokio::test]
    async fn test_rollback_starts_old_and_stops_current() {
        let config = test_config();
        let mut hosts = HashMap::new();
        let web1 = MockDockerHost::new("web1");

        // Gen 1: stopped (rollback target)
        web1.add_container(mock_container_summary(
            "old-1",
            "korgi-myapp-api-g1-0",
            "myapp",
            "api",
            1,
            0,
            "myapp/api:v1",
            ContainerSummaryStateEnum::EXITED,
            "Exited (0)",
        ));
        // Gen 2: running (current)
        web1.add_container(mock_container_summary(
            "current-1",
            "korgi-myapp-api-g2-0",
            "myapp",
            "api",
            2,
            0,
            "myapp/api:v2",
            ContainerSummaryStateEnum::RUNNING,
            "Up 5 min",
        ));

        // Image exists so no pull needed
        web1.add_existing_image("myapp/api:v1");
        hosts.insert("web1".to_string(), web1);

        rollback_service(&config, "api", &hosts).await.unwrap();

        let calls = hosts.get("web1").unwrap().get_calls();

        // Should start the old container
        let started: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                DockerCall::StartContainer { id } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert!(started.contains(&"old-1"), "Should start old gen container");

        // Should stop the current container
        let stopped: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                DockerCall::StopContainer { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            stopped.contains(&"current-1"),
            "Should stop current gen container"
        );
    }

    #[tokio::test]
    async fn test_rollback_pulls_missing_image() {
        let config = test_config();
        let mut hosts = HashMap::new();
        let web1 = MockDockerHost::new("web1");

        web1.add_container(mock_container_summary(
            "old-1",
            "korgi-myapp-api-g1-0",
            "myapp",
            "api",
            1,
            0,
            "myapp/api:v1",
            ContainerSummaryStateEnum::EXITED,
            "Exited (0)",
        ));
        web1.add_container(mock_container_summary(
            "current-1",
            "korgi-myapp-api-g2-0",
            "myapp",
            "api",
            2,
            0,
            "myapp/api:v2",
            ContainerSummaryStateEnum::RUNNING,
            "Up 5 min",
        ));

        // Image does NOT exist -- should be pulled
        hosts.insert("web1".to_string(), web1);

        rollback_service(&config, "api", &hosts).await.unwrap();

        let calls = hosts.get("web1").unwrap().get_calls();
        let pulled: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                DockerCall::PullImage { image } => Some(image.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            pulled.contains(&"myapp/api:v1"),
            "Should pull missing image"
        );
    }

    #[tokio::test]
    async fn test_rollback_no_previous_generation_fails() {
        let config = test_config();
        let mut hosts = HashMap::new();
        let web1 = MockDockerHost::new("web1");

        // Only current gen, no stopped gen to roll back to
        web1.add_container(mock_container_summary(
            "current-1",
            "korgi-myapp-api-g1-0",
            "myapp",
            "api",
            1,
            0,
            "myapp/api:v1",
            ContainerSummaryStateEnum::RUNNING,
            "Up 5 min",
        ));
        hosts.insert("web1".to_string(), web1);

        let result = rollback_service(&config, "api", &hosts).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No previous generation")
        );
    }

    #[tokio::test]
    async fn test_rollback_unknown_service_fails() {
        let config = test_config();
        let mut hosts = HashMap::new();
        hosts.insert("web1".to_string(), MockDockerHost::new("web1"));

        let result = rollback_service(&config, "nonexistent", &hosts).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_rollback_picks_most_recent_stopped_gen() {
        let config = test_config();
        let mut hosts = HashMap::new();
        let web1 = MockDockerHost::new("web1");

        // Gen 1: stopped
        web1.add_container(mock_container_summary(
            "gen1",
            "korgi-myapp-api-g1-0",
            "myapp",
            "api",
            1,
            0,
            "myapp/api:v1",
            ContainerSummaryStateEnum::EXITED,
            "Exited (0)",
        ));
        // Gen 2: stopped
        web1.add_container(mock_container_summary(
            "gen2",
            "korgi-myapp-api-g2-0",
            "myapp",
            "api",
            2,
            0,
            "myapp/api:v2",
            ContainerSummaryStateEnum::EXITED,
            "Exited (0)",
        ));
        // Gen 3: running (current)
        web1.add_container(mock_container_summary(
            "gen3",
            "korgi-myapp-api-g3-0",
            "myapp",
            "api",
            3,
            0,
            "myapp/api:v3",
            ContainerSummaryStateEnum::RUNNING,
            "Up 5 min",
        ));

        web1.add_existing_image("myapp/api:v2");
        hosts.insert("web1".to_string(), web1);

        rollback_service(&config, "api", &hosts).await.unwrap();

        let calls = hosts.get("web1").unwrap().get_calls();
        // Should start gen2 (most recent stopped), not gen1
        let started: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                DockerCall::StartContainer { id } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            started.contains(&"gen2"),
            "Should roll back to most recent stopped gen (2)"
        );
        assert!(!started.contains(&"gen1"), "Should NOT start older gen 1");
    }
}
