use anyhow::{Context, Result};
use std::collections::HashMap;
use tracing::debug;

use crate::cli::output;
use crate::config::types::Config;
use crate::docker::traits::DockerExecApi;
use crate::orchestrator::deploy::other_running;
use crate::orchestrator::state::LiveState;
use crate::orchestrator::traefik_sync::sync_traefik_config;
use crate::orchestrator::verify::{self, ContainerRef};

/// Roll back a service to the most recent stopped generation below the current
/// one.
///
/// Order: start the target generation -> health-check it -> sync Traefik (target
/// and current both routed) -> stop every other running generation -> sync again.
/// If the target does not become healthy it is stopped again and nothing else
/// changes; if the first sync fails nothing is stopped.
pub async fn rollback_service<D: DockerExecApi>(
    config: &Config,
    service: &str,
    docker_hosts: &HashMap<String, D>,
) -> Result<()> {
    let svc = config
        .services
        .iter()
        .find(|s| s.name == service)
        .context(format!("Service '{}' not found in config", service))?;
    let deploy_cfg = Config::deploy_config(svc);
    let state = LiveState::query(docker_hosts, &config.project.name).await?;

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

    // Health checks use the target generation's own image and settings.
    let mut target_svc = svc.clone();
    target_svc.image = rollback_containers[0].image.clone();

    // 1. Start the target generation (re-pulling garbage-collected images).
    let pb = output::spinner("Starting rollback containers...");
    let mut started: Vec<ContainerRef> = Vec::new();
    let start_result = async {
        for container in &rollback_containers {
            let docker = docker_hosts
                .get(&container.host_name)
                .context(format!("No connection to host {}", container.host_name))?;
            if container.is_running() {
                continue;
            }
            verify::ensure_image(docker, config, &container.image).await?;
            docker
                .start_container(&container.id)
                .await
                .with_context(|| {
                    format!(
                        "Failed to start rollback container {} on {}",
                        container.name, container.host_name
                    )
                })?;
            started.push(ContainerRef::from(*container));
            debug!("Started rollback container {}", container.name);
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    pb.finish_and_clear();

    // 2. Health-check the target generation before it gets any traffic.
    let all_target: Vec<ContainerRef> = rollback_containers
        .iter()
        .map(|c| ContainerRef::from(*c))
        .collect();
    let checked = match start_result {
        Ok(()) => {
            let pb = output::spinner("Waiting for rollback containers to become healthy...");
            let r = verify::verify_healthy(config, &target_svc, &all_target, docker_hosts).await;
            pb.finish_and_clear();
            r.context("rollback target failed its health check")
        }
        Err(e) => Err(e),
    };
    if let Err(e) = checked {
        output::error(&format!("Rollback aborted: {:#}", e));
        // Put the target back to stopped; it stays available as a rollback target.
        if let Err(stop_err) = verify::stop_all(docker_hosts, &started, 10).await {
            output::error(&format!(
                "Could not stop rollback containers: {:#}",
                stop_err
            ));
        }
        return Err(e.context(format!(
            "Rollback of '{}' aborted; generation {} still serving",
            service, current_gen
        )));
    }
    output::success(&format!(
        "Generation {} is running and healthy ({} containers)",
        rollback_gen,
        all_target.len()
    ));

    // 3. Route traffic to the target (alongside the current generation).
    sync_traefik_config(config, docker_hosts)
        .await
        .context("Traefik sync failed; current generation left running")?;

    // 4. Stop every other running generation (not just the one rolled back from).
    let state = LiveState::query(docker_hosts, &config.project.name).await?;
    let to_stop = other_running(&state, service, rollback_gen);
    let mut drain_error = None;
    if !to_stop.is_empty() {
        let pb = output::spinner("Stopping other generations...");
        let r = verify::stop_all(docker_hosts, &to_stop, deploy_cfg.drain_seconds as i64).await;
        pb.finish_and_clear();
        match r {
            Ok(()) => output::success(&format!("Stopped {} containers", to_stop.len())),
            Err(e) => {
                output::error(&format!("{:#}", e));
                drain_error = Some(e);
            }
        }
    }

    // 5. Remove the stopped containers from routing (always attempted).
    let final_sync = sync_traefik_config(config, docker_hosts).await;

    if let Some(e) = drain_error {
        return Err(e.context("Rollback completed with errors stopping old containers"));
    }
    final_sync.context("Final Traefik sync after rollback failed")?;

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
                secrets: None,
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
                DockerCall::PullImage { image, .. } => Some(image.as_str()),
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

    use crate::orchestrator::traefik_sync::{CONFIG_FILE, TRAEFIK_CONTAINER_NAME};

    fn traefik_config() -> Config {
        let mut config = test_config();
        config.hosts.push({
            let mut h = HostConfig::test_host("lb", "10.0.0.100");
            h.role = HostRole::Lb;
            h
        });
        config.traefik = Some(TraefikConfig {
            image: "traefik:v3.2".to_string(),
            hosts: vec![],
            entrypoints: HashMap::new(),
            network: "korgi-traefik".to_string(),
            acme: None,
        });
        let svc = &mut config.services[0];
        svc.routing = Some(RoutingConfig {
            rule: "Host(`api.example.com`)".to_string(),
            entrypoints: vec![],
            tls: false,
            lb_strategy: None,
            sticky: None,
        });
        svc.ports = Some(PortsConfig {
            container: 8080,
            host: None,
            host_base: Some(9001),
        });
        svc.deploy = Some(DeployConfig {
            drain_seconds: 1,
            start_delay: 0,
            rollback_keep: 2,
        });
        config
    }

    fn gen_container(
        id: &str,
        generation: u64,
        image: &str,
        running: bool,
        port: u16,
    ) -> bollard::models::ContainerSummary {
        let state = if running {
            ContainerSummaryStateEnum::RUNNING
        } else {
            ContainerSummaryStateEnum::EXITED
        };
        with_label(
            mock_container_summary(
                id,
                &format!("korgi-myapp-api-g{}-0", generation),
                "myapp",
                "api",
                generation,
                0,
                image,
                state,
                "",
            ),
            "korgi.host_port",
            &port.to_string(),
        )
    }

    fn setup(log: &SharedLog) -> HashMap<String, MockDockerHost> {
        let mut hosts = HashMap::new();
        let web1 = MockDockerHost::new("web1").with_shared_log(log);
        web1.add_container(gen_container("g1", 1, "myapp/api:v1", false, 9001));
        web1.add_container(gen_container("g2", 2, "myapp/api:v2", true, 9002));
        web1.add_existing_image("myapp/api:v1");
        hosts.insert("web1".to_string(), web1);
        hosts.insert(
            "lb".to_string(),
            MockDockerHost::new("lb").with_shared_log(log),
        );
        hosts
    }

    fn index_of(log: &SharedLog, pred: impl Fn(&DockerCall) -> bool) -> Vec<usize> {
        log.lock()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, (_, c))| pred(c))
            .map(|(i, _)| i)
            .collect()
    }

    #[tokio::test]
    async fn test_rollback_order_start_health_sync_stop_sync() {
        let config = traefik_config();
        let log = SharedLog::default();
        let hosts = setup(&log);

        rollback_service(&config, "api", &hosts).await.unwrap();

        let start = index_of(
            &log,
            |c| matches!(c, DockerCall::StartContainer { id } if id == "g1"),
        )[0];
        let health = index_of(
            &log,
            |c| matches!(c, DockerCall::InspectContainer { id } if id == "g1"),
        );
        let writes = index_of(
            &log,
            |c| matches!(c, DockerCall::Exec { script, .. } if script.contains("mv -f")),
        );
        let stop = index_of(
            &log,
            |c| matches!(c, DockerCall::StopContainer { id, .. } if id == "g2"),
        )[0];

        assert!(!health.is_empty(), "target health-checked");
        assert_eq!(writes.len(), 2, "synced before and after stopping");
        assert!(start < health[0]);
        assert!(*health.last().unwrap() < writes[0], "healthy before routed");
        assert!(
            writes[0] < stop,
            "routed before the current generation stops"
        );
        assert!(stop < writes[1], "final sync after stop");

        let yaml = hosts["lb"]
            .file(TRAEFIK_CONTAINER_NAME, CONFIG_FILE)
            .unwrap();
        assert!(yaml.contains("http://10.0.0.1:9001\""), "{}", yaml);
        assert!(!yaml.contains(":9002"), "{}", yaml);
    }

    #[tokio::test]
    async fn test_rollback_health_failure_keeps_current_serving() {
        let mut config = traefik_config();
        config.services[0].health = Some(HealthConfig {
            mode: HealthMode::Docker,
            path: "/health".into(),
            interval: "1s".into(),
            timeout: "1s".into(),
            retries: 1,
            start_period: None,
        });
        let log = SharedLog::default();
        let hosts = setup(&log);
        hosts["web1"].set_health_status(Some(bollard::models::HealthStatusEnum::UNHEALTHY));

        let err = rollback_service(&config, "api", &hosts).await.unwrap_err();
        assert!(format!("{:#}", err).contains("health"), "{:#}", err);
        let stopped = index_of(&log, |c| matches!(c, DockerCall::StopContainer { .. }));
        // Only the rollback target is put back to stopped; gen 2 untouched.
        assert_eq!(stopped.len(), 1);
        assert!(
            index_of(
                &log,
                |c| matches!(c, DockerCall::StopContainer { id, .. } if id == "g2")
            )
            .is_empty()
        );
        assert!(
            index_of(&log, |c| matches!(c, DockerCall::Exec { .. })).is_empty(),
            "never routed"
        );
    }

    #[tokio::test]
    async fn test_rollback_sync_failure_does_not_stop_current() {
        let config = traefik_config();
        let log = SharedLog::default();
        let hosts = setup(&log);
        hosts["lb"].set_inspect_error(TRAEFIK_CONTAINER_NAME);
        assert!(rollback_service(&config, "api", &hosts).await.is_err());
        assert!(index_of(&log, |c| matches!(c, DockerCall::StopContainer { .. })).is_empty());
    }

    #[tokio::test]
    async fn test_rollback_stops_every_other_running_generation() {
        let config = traefik_config();
        let log = SharedLog::default();
        let hosts = setup(&log);
        // A leftover gen 3 is also running (e.g. a drain that failed earlier).
        hosts["web1"].add_container(gen_container("g3", 3, "myapp/api:v3", true, 9003));
        hosts["web1"].add_container(gen_container("g0", 0, "myapp/api:v0", false, 9000));
        rollback_service(&config, "api", &hosts).await.unwrap();
        let state = LiveState::query(&hosts, "myapp").await.unwrap();
        // current gen is 3 -> target is highest stopped below 3 = gen 1
        assert_eq!(state.running_generations("api"), vec![1]);
    }

    #[tokio::test]
    async fn test_second_rollback_runs_one_generation_only() {
        let config = traefik_config();
        let log = SharedLog::default();
        let hosts = setup(&log);
        hosts["web1"].add_container(gen_container("g0", 0, "myapp/api:v0", false, 9000));

        rollback_service(&config, "api", &hosts).await.unwrap(); // 2 -> 1
        let state = LiveState::query(&hosts, "myapp").await.unwrap();
        assert_eq!(state.current_generation("api"), Some(1));
        assert_eq!(state.rollback_generation("api"), Some(0));

        hosts["web1"].add_existing_image("myapp/api:v0");
        rollback_service(&config, "api", &hosts).await.unwrap(); // 1 -> 0
        let state = LiveState::query(&hosts, "myapp").await.unwrap();
        assert_eq!(state.running_generations("api"), vec![0]);
        assert_eq!(state.next_generation("api"), 3);
    }

    #[tokio::test]
    async fn test_rollback_pulls_with_registry_credentials() {
        let mut config = traefik_config();
        config.registries = vec![RegistryConfig {
            url: Some("ghcr.io".to_string()),
            username: Some("u".to_string()),
            password: Some("p".to_string()),
            github_token: None,
        }];
        let log = SharedLog::default();
        let hosts = setup(&log);
        hosts["web1"].containers.lock().unwrap().clear();
        hosts["web1"].add_container(gen_container("g1", 1, "ghcr.io/me/api:v1", false, 9001));
        hosts["web1"].add_container(gen_container("g2", 2, "ghcr.io/me/api:v2", true, 9002));

        rollback_service(&config, "api", &hosts).await.unwrap();
        assert!(hosts["web1"].get_calls().contains(&DockerCall::PullImage {
            image: "ghcr.io/me/api:v1".to_string(),
            with_auth: true,
        }));
    }
}
