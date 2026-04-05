use anyhow::{Context, Result};
use std::collections::HashMap;
use tracing::{debug, info};

use crate::cli::output;
use crate::config::interpolate;
use crate::config::types::{Config, ServiceConfig};
use crate::docker::containers::{self, KorgiContainer};
use crate::docker::labels;
use crate::docker::registry;
use crate::docker::traits::DockerHostApi;
use crate::health;
use crate::orchestrator::placement;
use crate::orchestrator::state::LiveState;

/// Execute the zero-downtime deployment pipeline for a service.
pub async fn deploy_service<D: DockerHostApi>(
    config: &Config,
    svc: &ServiceConfig,
    image_override: Option<&str>,
    docker_hosts: &HashMap<String, D>,
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

    let placements = placement::place_replicas(&matching_hosts.to_vec(), svc.replicas);

    info!(
        "Generation {} → {} replicas across {} hosts",
        generation,
        svc.replicas,
        matching_hosts.len()
    );

    if dry_run {
        output::info("Dry run -- would deploy:");
        for (host, instance) in &placements {
            let name =
                labels::container_name(&config.project.name, &svc.name, generation, *instance);
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
        docker
            .pull_image(&image, auth)
            .await
            .with_context(|| format!("Failed to pull {} on {}", image, host_name))?;
    }
    pb.finish_and_clear();
    output::success(&format!("Image pulled on {} hosts", unique_hosts.len()));

    // Phase 2.5: FIND AVAILABLE PORT RANGE
    let port_offset = if let Some(ports) = &svc.ports
        && let Some(base) = ports.host_base
    {
        // Collect all used ports across target hosts
        let mut used_ports = std::collections::HashSet::new();
        for host_name in &unique_hosts {
            let docker = docker_hosts.get(*host_name).unwrap();
            let all_containers = docker
                .list_containers(std::collections::HashMap::new(), false)
                .await?;
            for c in &all_containers {
                if let Some(c_ports) = &c.ports {
                    for p in c_ports {
                        if let Some(port) = p.public_port {
                            used_ports.insert(port);
                        }
                    }
                }
            }
        }
        let offset = find_free_port_offset(base, svc.replicas, generation, &used_ports)?;
        debug!(
            "Port range: {}..{}",
            base + offset,
            base + offset + svc.replicas as u16 - 1
        );
        Some(offset)
    } else {
        None
    };

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
        let container_name =
            labels::container_name(&config.project.name, &svc.name, generation, *instance);

        let container_config = containers::build_container_config(
            &config.project.name,
            &svc_for_deploy,
            generation,
            *instance,
            traefik_network,
            &resolved_env,
            Some(host.internal_addr()),
            port_offset,
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
    if let Some(health_cfg) = &svc.health {
        let pb = output::spinner("Waiting for health checks...");
        let timeout = std::time::Duration::from_secs(deploy_cfg.drain_seconds * 2);

        for (idx, (host_name, container_id)) in new_container_ids.iter().enumerate() {
            let docker = docker_hosts.get(host_name).unwrap();

            // Build HTTP check info if mode=http
            let http_check = if health_cfg.mode == crate::config::types::HealthMode::Http {
                let host_cfg = config.hosts.iter().find(|h| &h.name == host_name);
                let addr = host_cfg.map(|h| h.internal_addr()).unwrap_or("127.0.0.1");
                let port = svc
                    .ports
                    .as_ref()
                    .and_then(|p| {
                        p.host_base
                            .map(|base| {
                                let gen_offset =
                                    (generation.saturating_sub(1) as u16) * svc.replicas as u16;
                                base + gen_offset + placements[idx].1 as u16
                            })
                            .or(p.host)
                    })
                    .unwrap_or(svc.ports.as_ref().map(|p| p.container).unwrap_or(80));
                Some(health::HttpHealthCheck {
                    url: format!("http://{}:{}{}", addr, port, health_cfg.path),
                    interval: std::time::Duration::from_secs(2),
                    host_name,
                })
            } else {
                None
            };

            match health::wait_healthy(docker, container_id, timeout, http_check).await {
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
        // No health check -- wait for start_delay
        let delay = deploy_cfg.start_delay;
        let pb = output::spinner(&format!("Waiting {}s for containers to start...", delay));
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        pb.finish_and_clear();
        output::success("Start delay elapsed");
    }

    // Phase 5: DRAIN OLD
    let old_generation = generation.checked_sub(1);
    if let Some(old_gen) = old_generation {
        let old_containers: Vec<&KorgiContainer> = state
            .generation_containers(&svc.name, old_gen)
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

/// Find the first port offset where all replicas fit without colliding with used ports.
/// Starts from the generation-based default offset, advances by replica count until free.
pub fn find_free_port_offset(
    base: u16,
    replicas: u32,
    generation: u64,
    used_ports: &std::collections::HashSet<u16>,
) -> Result<u16> {
    let replicas = replicas as u16;
    let mut offset = (generation.saturating_sub(1) as u16) * replicas;
    loop {
        let all_free = (0..replicas).all(|i| !used_ports.contains(&(base + offset + i)));
        if all_free {
            return Ok(offset);
        }
        offset += replicas;
        if offset > 10000 {
            anyhow::bail!(
                "Could not find {} free ports starting from {}",
                replicas,
                base
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::*;
    use crate::docker::mock::tests::*;
    use bollard::models::ContainerSummaryStateEnum;
    use std::collections::HashSet;

    fn test_config() -> Config {
        Config {
            project: ProjectConfig {
                name: "myapp".to_string(),
                secrets: None,
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

    fn mock_hosts() -> HashMap<String, MockDockerHost> {
        let mut hosts = HashMap::new();
        hosts.insert("web1".to_string(), MockDockerHost::new("web1"));
        hosts.insert("web2".to_string(), MockDockerHost::new("web2"));
        hosts
    }

    // --- Dry Run ---

    #[tokio::test]
    async fn test_deploy_dry_run_makes_no_changes() {
        let config = test_config();
        let svc = &config.services[0];
        let hosts = mock_hosts();

        deploy_service(&config, svc, None, &hosts, true)
            .await
            .unwrap();

        // Dry run should only list containers (state query), not create/start anything
        let web1_calls = hosts.get("web1").unwrap().get_calls();
        let web2_calls = hosts.get("web2").unwrap().get_calls();
        let all_calls: Vec<_> = web1_calls.iter().chain(web2_calls.iter()).collect();

        // Should have list_containers calls (for state query) but no create/start
        assert!(
            all_calls
                .iter()
                .any(|c| matches!(c, DockerCall::ListContainers { .. }))
        );
        assert!(
            !all_calls
                .iter()
                .any(|c| matches!(c, DockerCall::CreateContainer { .. }))
        );
        assert!(
            !all_calls
                .iter()
                .any(|c| matches!(c, DockerCall::StartContainer { .. }))
        );
        assert!(
            !all_calls
                .iter()
                .any(|c| matches!(c, DockerCall::PullImage { .. }))
        );
    }

    // --- First Deploy (no existing containers) ---

    #[tokio::test]
    async fn test_first_deploy_creates_containers() {
        let config = test_config();
        let svc = &config.services[0];
        let hosts = mock_hosts();

        deploy_service(&config, svc, None, &hosts, false)
            .await
            .unwrap();

        // Should pull image on both hosts
        let all_calls: Vec<_> = hosts.values().flat_map(|h| h.get_calls()).collect();
        let pull_count = all_calls
            .iter()
            .filter(|c| matches!(c, DockerCall::PullImage { .. }))
            .count();
        assert!(pull_count >= 1, "Should pull image on at least one host");

        // Should create 2 containers (replicas=2)
        let create_count = all_calls
            .iter()
            .filter(|c| matches!(c, DockerCall::CreateContainer { .. }))
            .count();
        assert_eq!(create_count, 2, "Should create 2 containers for 2 replicas");

        // Should start 2 containers
        let start_count = all_calls
            .iter()
            .filter(|c| matches!(c, DockerCall::StartContainer { .. }))
            .count();
        assert_eq!(start_count, 2, "Should start 2 containers");

        // Should ensure network on target hosts
        let network_count = all_calls
            .iter()
            .filter(|c| matches!(c, DockerCall::EnsureNetwork { .. }))
            .count();
        assert!(network_count >= 1, "Should ensure network exists");
    }

    // --- Deploy with image override ---

    #[tokio::test]
    async fn test_deploy_with_image_override() {
        let config = test_config();
        let svc = &config.services[0];
        let hosts = mock_hosts();

        deploy_service(&config, svc, Some("myapp/api:v2"), &hosts, false)
            .await
            .unwrap();

        let all_calls: Vec<_> = hosts.values().flat_map(|h| h.get_calls()).collect();

        // Should pull the overridden image, not the original
        let pull_calls: Vec<_> = all_calls
            .iter()
            .filter_map(|c| match c {
                DockerCall::PullImage { image } => Some(image.as_str()),
                _ => None,
            })
            .collect();
        assert!(pull_calls.iter().all(|&img| img == "myapp/api:v2"));
    }

    // --- Deploy drains old generation ---

    #[tokio::test]
    async fn test_deploy_drains_old_generation() {
        let config = test_config();
        let svc = &config.services[0];
        let hosts = mock_hosts();

        // Add existing gen-1 containers to both hosts
        hosts
            .get("web1")
            .unwrap()
            .add_container(mock_container_summary(
                "old-1",
                "korgi-myapp-api-g1-0",
                "myapp",
                "api",
                1,
                0,
                "myapp/api:v1",
                ContainerSummaryStateEnum::RUNNING,
                "Up 5 minutes (healthy)",
            ));
        hosts
            .get("web2")
            .unwrap()
            .add_container(mock_container_summary(
                "old-2",
                "korgi-myapp-api-g1-1",
                "myapp",
                "api",
                1,
                1,
                "myapp/api:v1",
                ContainerSummaryStateEnum::RUNNING,
                "Up 5 minutes (healthy)",
            ));

        deploy_service(&config, svc, None, &hosts, false)
            .await
            .unwrap();

        // Should stop old containers
        let all_calls: Vec<_> = hosts.values().flat_map(|h| h.get_calls()).collect();
        let stop_calls: Vec<_> = all_calls
            .iter()
            .filter_map(|c| match c {
                DockerCall::StopContainer { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            stop_calls.contains(&"old-1"),
            "Should stop old container on web1"
        );
        assert!(
            stop_calls.contains(&"old-2"),
            "Should stop old container on web2"
        );
    }

    // --- Deploy with health check failure rolls back ---

    #[tokio::test]
    async fn test_deploy_health_check_failure_cleans_up() {
        let mut config = test_config();
        config.services[0].health = Some(HealthConfig {
            mode: Default::default(),
            path: "/health".to_string(),
            interval: "1s".to_string(),
            timeout: "1s".to_string(),
            retries: 1,
            start_period: None,
        });
        // Set drain_seconds low so timeout is low (health timeout = drain_seconds * 2)
        config.services[0].deploy = Some(DeployConfig {
            drain_seconds: 1,
            start_delay: 1,
            rollback_keep: 2,
        });
        let svc = &config.services[0];
        let hosts = mock_hosts();

        // Make health check return unhealthy
        for host in hosts.values() {
            host.set_health_status(Some(bollard::models::HealthStatusEnum::UNHEALTHY));
        }

        let result = deploy_service(&config, svc, None, &hosts, false).await;
        assert!(
            result.is_err(),
            "Deploy should fail on health check failure"
        );

        // Should have attempted to stop and remove the new containers (cleanup)
        let all_calls: Vec<_> = hosts.values().flat_map(|h| h.get_calls()).collect();
        let stop_count = all_calls
            .iter()
            .filter(|c| matches!(c, DockerCall::StopContainer { .. }))
            .count();
        let remove_count = all_calls
            .iter()
            .filter(|c| matches!(c, DockerCall::RemoveContainer { .. }))
            .count();
        assert!(
            stop_count > 0,
            "Should stop new containers on health failure"
        );
        assert!(
            remove_count > 0,
            "Should remove new containers on health failure"
        );
    }

    // --- Deploy with no matching hosts fails ---

    #[tokio::test]
    async fn test_deploy_no_matching_hosts() {
        let mut config = test_config();
        config.services[0].placement_labels = vec!["gpu".to_string()]; // No hosts have "gpu"
        let svc = &config.services[0];
        let hosts = mock_hosts();

        let result = deploy_service(&config, svc, None, &hosts, false).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No hosts match"));
    }

    // --- Deploy cleanup removes old stopped generations ---

    #[tokio::test]
    async fn test_deploy_cleans_up_old_generations() {
        let config = test_config();
        let svc = &config.services[0];
        let hosts = mock_hosts();

        // Add old stopped containers from gen 1 and running gen 3
        // With rollback_keep=2, gen 1 should be cleaned (gen 4 - 2 - 1 = 1)
        hosts
            .get("web1")
            .unwrap()
            .add_container(mock_container_summary(
                "ancient",
                "korgi-myapp-api-g1-0",
                "myapp",
                "api",
                1,
                0,
                "myapp/api:v1",
                ContainerSummaryStateEnum::EXITED,
                "Exited (0)",
            ));
        hosts
            .get("web1")
            .unwrap()
            .add_container(mock_container_summary(
                "current-0",
                "korgi-myapp-api-g3-0",
                "myapp",
                "api",
                3,
                0,
                "myapp/api:v1",
                ContainerSummaryStateEnum::RUNNING,
                "Up 5 minutes (healthy)",
            ));
        hosts
            .get("web2")
            .unwrap()
            .add_container(mock_container_summary(
                "current-1",
                "korgi-myapp-api-g3-1",
                "myapp",
                "api",
                3,
                1,
                "myapp/api:v1",
                ContainerSummaryStateEnum::RUNNING,
                "Up 5 minutes (healthy)",
            ));

        deploy_service(&config, svc, None, &hosts, false)
            .await
            .unwrap();

        // Should remove the ancient stopped container
        let web1_calls = hosts.get("web1").unwrap().get_calls();
        let removed: Vec<_> = web1_calls
            .iter()
            .filter_map(|c| match c {
                DockerCall::RemoveContainer { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            removed.contains(&"ancient"),
            "Should clean up old gen 1 container"
        );
    }

    // --- Deploy aborts cleanly on pull failure ---

    #[tokio::test]
    async fn test_deploy_pull_failure_aborts_no_containers_created() {
        let config = test_config();
        let svc = &config.services[0];
        let hosts = mock_hosts();

        // Make pull fail on web1
        hosts
            .get("web1")
            .unwrap()
            .set_pull_error("registry unavailable");

        let result = deploy_service(&config, svc, None, &hosts, false).await;
        assert!(result.is_err(), "Deploy should fail when pull fails");

        // No containers should have been created on any host
        let all_calls: Vec<_> = hosts.values().flat_map(|h| h.get_calls()).collect();
        assert!(
            !all_calls
                .iter()
                .any(|c| matches!(c, DockerCall::CreateContainer { .. })),
            "No containers should be created when pull fails"
        );
        assert!(
            !all_calls
                .iter()
                .any(|c| matches!(c, DockerCall::StartContainer { .. })),
            "No containers should be started when pull fails"
        );
    }

    // --- Port allocation ---

    #[test]
    fn test_port_offset_gen1_no_conflicts() {
        // Gen 1, 2 replicas, base 9001, no ports used
        let used = HashSet::new();
        let offset = find_free_port_offset(9001, 2, 1, &used).unwrap();
        assert_eq!(offset, 0); // gen 1 offset = (1-1)*2 = 0
        // Ports: 9001, 9002
    }

    #[test]
    fn test_port_offset_gen2_no_conflicts() {
        // Gen 2, 2 replicas, base 9001, gen1 ports still used
        let used: HashSet<u16> = [9001, 9002].into();
        let offset = find_free_port_offset(9001, 2, 2, &used).unwrap();
        assert_eq!(offset, 2); // gen 2 offset = (2-1)*2 = 2
        // Ports: 9003, 9004
    }

    #[test]
    fn test_port_offset_gen2_gen1_freed() {
        // Gen 2, but gen1 ports already freed (old containers stopped)
        let used = HashSet::new();
        let offset = find_free_port_offset(9001, 2, 2, &used).unwrap();
        assert_eq!(offset, 2); // still uses gen-based default offset
        // Ports: 9003, 9004
    }

    #[test]
    fn test_port_offset_skips_occupied_range() {
        // Gen 2, base 9001, 2 replicas. Default would be 9003,9004 but 9003 is taken
        let used: HashSet<u16> = [9001, 9002, 9003].into();
        let offset = find_free_port_offset(9001, 2, 2, &used).unwrap();
        assert_eq!(offset, 4); // skips to 9005, 9006
    }

    #[test]
    fn test_port_offset_skips_multiple_occupied_ranges() {
        // Several ranges occupied
        let used: HashSet<u16> = [9001, 9002, 9003, 9004, 9005, 9006].into();
        let offset = find_free_port_offset(9001, 2, 1, &used).unwrap();
        assert_eq!(offset, 6); // 9007, 9008
    }

    #[test]
    fn test_port_offset_three_replicas() {
        // 3 replicas, gen 3
        let used: HashSet<u16> = [9001, 9002, 9003, 9004, 9005, 9006].into();
        let offset = find_free_port_offset(9001, 3, 3, &used).unwrap();
        assert_eq!(offset, 6); // gen 3 default = (3-1)*3 = 6 -> 9007,9008,9009
    }

    #[test]
    fn test_port_offset_gen3_after_gen1_freed_gen2_running() {
        // Simulates: gen1 drained (freed), gen2 running, deploying gen3
        // Gen2 occupies 9003, 9004 (offset 2, 2 replicas)
        let used: HashSet<u16> = [9003, 9004].into();
        let offset = find_free_port_offset(9001, 2, 3, &used).unwrap();
        assert_eq!(offset, 4); // gen 3 default = (3-1)*2 = 4 -> 9005, 9006 (free)
    }

    #[test]
    fn test_port_offset_external_service_occupying_port() {
        // Some non-korgi service using port 9005
        let used: HashSet<u16> = [9001, 9002, 9005].into();
        let offset = find_free_port_offset(9001, 2, 2, &used).unwrap();
        assert_eq!(offset, 2); // gen 2 default = 2 -> 9003, 9004 (free, 9005 not in range)
    }

    #[test]
    fn test_port_offset_external_blocks_default_range() {
        // External service blocks the gen2 default range
        let used: HashSet<u16> = [9001, 9002, 9003].into();
        let offset = find_free_port_offset(9001, 2, 2, &used).unwrap();
        // Default offset 2 -> 9003,9004. 9003 taken, skip to offset 4 -> 9005,9006
        assert_eq!(offset, 4);
    }

    #[test]
    fn test_port_offset_reuses_freed_ports() {
        // After many deploys, old gen ports are freed.
        // Gen 5, base 9001, 2 replicas. Only gen 4 (9007,9008) still running.
        let used: HashSet<u16> = [9007, 9008].into();
        let offset = find_free_port_offset(9001, 2, 5, &used).unwrap();
        // Default offset = (5-1)*2 = 8 -> 9009, 9010 (free)
        assert_eq!(offset, 8);
    }

    #[test]
    fn test_port_offset_wraps_around_to_freed_range() {
        // Gen 10, base 9001, 2 replicas. Only gen 9 running.
        // Default offset = 18 -> 9019, 9020
        let used: HashSet<u16> = [9017, 9018].into();
        let offset = find_free_port_offset(9001, 2, 10, &used).unwrap();
        assert_eq!(offset, 18); // 9019, 9020 are free
    }
}
