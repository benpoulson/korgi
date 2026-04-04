use anyhow::{Context, Result};
use std::collections::HashMap;
use tracing::debug;

use crate::cli::output;
use crate::config::interpolate;
use crate::config::types::Config;
use crate::docker::containers;
use crate::docker::labels;
use crate::docker::traits::DockerHostApi;
use crate::orchestrator::placement;
use crate::orchestrator::state::LiveState;

/// Scale a service to a target replica count.
pub async fn scale_service<D: DockerHostApi>(
    config: &Config,
    service: &str,
    target_count: u32,
    docker_hosts: &HashMap<String, D>,
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
            &matching_hosts.to_vec(),
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
                Some(host.internal_addr()),
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
        to_remove.sort_by_key(|c| std::cmp::Reverse(c.instance));
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
            hosts: vec![
                {
                    let mut h = HostConfig::test_host("web1", "10.0.0.1");
                    h.labels = vec!["web".to_string()];
                    h
                },
                {
                    let mut h = HostConfig::test_host("web2", "10.0.0.2");
                    h.labels = vec!["web".to_string()];
                    h
                },
            ],
            traefik: Some(TraefikConfig {
                image: "traefik:v3.2".to_string(),
                hosts: vec!["web1".to_string()],
                entrypoints: HashMap::new(),
                network: "korgi-traefik".to_string(),
                acme: None,
            }),
            services: vec![{
                let mut svc = ServiceConfig::test_service("api", "myapp/api:v1");
                svc.replicas = 2;
                svc.placement_labels = vec!["web".to_string()];
                svc
            }],
        }
    }

    fn mock_hosts_with_running(generation: u64) -> HashMap<String, MockDockerHost> {
        let mut hosts = HashMap::new();
        let web1 = MockDockerHost::new("web1");
        web1.add_container(mock_container_summary(
            "c1", "korgi-myapp-api-g1-0", "myapp", "api", generation, 0,
            "myapp/api:v1", ContainerSummaryStateEnum::RUNNING, "Up 5 minutes",
        ));
        let web2 = MockDockerHost::new("web2");
        web2.add_container(mock_container_summary(
            "c2", "korgi-myapp-api-g1-1", "myapp", "api", generation, 1,
            "myapp/api:v1", ContainerSummaryStateEnum::RUNNING, "Up 5 minutes",
        ));
        hosts.insert("web1".to_string(), web1);
        hosts.insert("web2".to_string(), web2);
        hosts
    }

    #[tokio::test]
    async fn test_scale_up_creates_new_containers() {
        let config = test_config();
        let hosts = mock_hosts_with_running(1);

        scale_service(&config, "api", 4, &hosts).await.unwrap();

        let all_calls: Vec<_> = hosts.values()
            .flat_map(|h| h.get_calls())
            .collect();
        // Should create 2 new containers (scaling from 2 to 4)
        let create_count = all_calls.iter()
            .filter(|c| matches!(c, DockerCall::CreateContainer { .. }))
            .count();
        assert_eq!(create_count, 2, "Should create 2 new containers");

        let start_count = all_calls.iter()
            .filter(|c| matches!(c, DockerCall::StartContainer { .. }))
            .count();
        assert_eq!(start_count, 2, "Should start 2 new containers");
    }

    #[tokio::test]
    async fn test_scale_down_removes_containers() {
        let config = test_config();
        let hosts = mock_hosts_with_running(1);

        scale_service(&config, "api", 1, &hosts).await.unwrap();

        let all_calls: Vec<_> = hosts.values()
            .flat_map(|h| h.get_calls())
            .collect();
        // Should stop 1 container (scaling from 2 to 1)
        let stop_count = all_calls.iter()
            .filter(|c| matches!(c, DockerCall::StopContainer { .. }))
            .count();
        assert_eq!(stop_count, 1, "Should stop 1 container");

        // Should remove 1 container
        let remove_count = all_calls.iter()
            .filter(|c| matches!(c, DockerCall::RemoveContainer { .. }))
            .count();
        assert_eq!(remove_count, 1, "Should remove 1 container");
    }

    #[tokio::test]
    async fn test_scale_same_count_noop() {
        let config = test_config();
        let hosts = mock_hosts_with_running(1);

        scale_service(&config, "api", 2, &hosts).await.unwrap();

        let all_calls: Vec<_> = hosts.values()
            .flat_map(|h| h.get_calls())
            .collect();
        // Should not create or stop anything
        assert!(!all_calls.iter().any(|c| matches!(c, DockerCall::CreateContainer { .. })));
        assert!(!all_calls.iter().any(|c| matches!(c, DockerCall::StopContainer { .. })));
    }

    #[tokio::test]
    async fn test_scale_nonexistent_service_fails() {
        let config = test_config();
        let hosts = mock_hosts_with_running(1);

        let result = scale_service(&config, "nonexistent", 3, &hosts).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_scale_down_removes_highest_instances_first() {
        let config = test_config();
        let mut hosts = HashMap::new();
        let web1 = MockDockerHost::new("web1");
        // Add 3 containers: instance 0, 1, 2 all on web1
        for i in 0..3 {
            web1.add_container(mock_container_summary(
                &format!("c{}", i), &format!("korgi-myapp-api-g1-{}", i),
                "myapp", "api", 1, i,
                "myapp/api:v1", ContainerSummaryStateEnum::RUNNING, "Up",
            ));
        }
        hosts.insert("web1".to_string(), web1);
        hosts.insert("web2".to_string(), MockDockerHost::new("web2"));

        scale_service(&config, "api", 1, &hosts).await.unwrap();

        // Should remove instance 2 and 1 (highest first), keeping instance 0
        let web1_calls = hosts.get("web1").unwrap().get_calls();
        let stopped: Vec<_> = web1_calls.iter()
            .filter_map(|c| match c {
                DockerCall::StopContainer { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(stopped.len(), 2);
        assert!(stopped.contains(&"c2"), "Should stop highest instance first");
        assert!(stopped.contains(&"c1"), "Should stop second highest instance");
    }

    #[tokio::test]
    async fn test_scale_to_zero_removes_all() {
        let config = test_config();
        let hosts = mock_hosts_with_running(1);

        scale_service(&config, "api", 0, &hosts).await.unwrap();

        let all_calls: Vec<_> = hosts.values()
            .flat_map(|h| h.get_calls())
            .collect();
        // Should stop both containers
        let stop_count = all_calls.iter()
            .filter(|c| matches!(c, DockerCall::StopContainer { .. }))
            .count();
        assert_eq!(stop_count, 2, "Should stop all 2 containers when scaling to 0");

        // Should remove both containers
        let remove_count = all_calls.iter()
            .filter(|c| matches!(c, DockerCall::RemoveContainer { .. }))
            .count();
        assert_eq!(remove_count, 2, "Should remove all 2 containers when scaling to 0");
    }
}
