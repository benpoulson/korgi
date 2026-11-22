use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use tracing::{debug, info};

use crate::cli::output;
use crate::config::interpolate;
use crate::config::types::{Config, ServiceConfig};
use crate::docker::containers::{self, KorgiContainer};
use crate::docker::labels;
use crate::docker::registry;
use crate::docker::traits::DockerHostApi;
use crate::orchestrator::placement;
use crate::orchestrator::ports;
use crate::orchestrator::state::LiveState;
use crate::orchestrator::verify::{self, ContainerRef, CreatedContainers};

/// Execute the start + health-check part of the zero-downtime pipeline for a
/// service. Returns the new generation number (needed for the drain phase).
///
/// On any failure after the first container was created, every container this
/// call created is removed before the error is returned; the old generation is
/// never touched here.
pub async fn deploy_service<D: DockerHostApi>(
    config: &Config,
    svc: &ServiceConfig,
    image_override: Option<&str>,
    docker_hosts: &HashMap<String, D>,
    dry_run: bool,
) -> Result<Option<u64>> {
    let created = CreatedContainers::new();
    deploy_service_tracked(config, svc, image_override, docker_hosts, dry_run, &created).await
}

/// Like [`deploy_service`], recording every created container in `created` so a
/// caller that cancels this future (Ctrl-C) can still remove them.
pub async fn deploy_service_tracked<D: DockerHostApi>(
    config: &Config,
    svc: &ServiceConfig,
    image_override: Option<&str>,
    docker_hosts: &HashMap<String, D>,
    dry_run: bool,
    created: &CreatedContainers,
) -> Result<Option<u64>> {
    let image = image_override.unwrap_or(&svc.image);
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
        return Ok(None);
    }

    let mut unique_hosts: Vec<&str> = placements
        .iter()
        .map(|(h, _)| h.name.as_str())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    unique_hosts.sort_unstable();
    for host_name in &unique_hosts {
        docker_hosts
            .get(*host_name)
            .context(format!("No Docker connection for host {}", host_name))?;
    }

    // Phase 2: PULL (parallel across unique hosts)
    let pb = output::spinner("Pulling image on target hosts...");
    let auth = registry::credentials_for_image(image, &config.registries);
    let pulls = unique_hosts.iter().map(|host_name| {
        let docker = &docker_hosts[*host_name];
        let auth = auth.clone();
        async move {
            docker
                .pull_image(image, auth)
                .await
                .with_context(|| format!("Failed to pull {} on {}", image, host_name))
        }
    });
    let pulled = verify::collect_errors(futures::future::join_all(pulls).await);
    pb.finish_and_clear();
    pulled?;
    output::success(&format!("Image pulled on {} hosts", unique_hosts.len()));

    // Phase 2.5: HOST PORTS -- pick a free block, reserving ports of every korgi
    // container on the target hosts (stopped rollback targets included).
    let host_ports: Vec<Option<u16>> = match svc.ports.as_ref().and_then(|p| p.host_base) {
        Some(base) => {
            let used = ports::collect_used_ports_on(docker_hosts, &unique_hosts).await?;
            let offset = ports::find_free_port_offset(base, svc.replicas, generation, &used)?;
            let ports = placements
                .iter()
                .map(|(_, instance)| ports::block_port(base, offset, *instance).map(Some))
                .collect::<Result<Vec<_>>>()?;
            debug!("Host ports for generation {}: {:?}", generation, ports);
            ports
        }
        None => vec![None; placements.len()],
    };

    // Resolve environment variables
    let sys_env = interpolate::system_env();
    let resolved_env = interpolate::interpolate_env(&svc.env, &sys_env)?;

    // Build a modified service config with the potentially overridden image
    let mut svc_for_deploy = svc.clone();
    svc_for_deploy.image = image.to_string();

    // Phases 3-4: from here on, any failure removes what this call created.
    let started = async {
        start_green(
            config,
            &svc_for_deploy,
            generation,
            &placements,
            &host_ports,
            traefik_network,
            &resolved_env,
            &unique_hosts,
            docker_hosts,
            created,
        )
        .await?;

        let pb = output::spinner("Waiting for health checks...");
        let new_containers = created.for_service(&svc.name);
        let healthy =
            verify::verify_healthy(config, &svc_for_deploy, &new_containers, docker_hosts).await;
        pb.finish_and_clear();
        healthy.context("health check failed")?;
        output::success("All containers healthy");
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(e) = started {
        output::error(&format!("Deploy of '{}' failed: {:#}", svc.name, e));
        let ours = created.take_service(&svc.name);
        verify::discard_created(docker_hosts, &ours).await;
        return Err(e.context(format!(
            "Deploy aborted for '{}'; old generation untouched",
            svc.name
        )));
    }

    output::success(&format!(
        "Deployed {} generation {} ({} replicas) -- awaiting Traefik sync before drain",
        svc.name, generation, svc.replicas
    ));

    Ok(Some(generation))
}

#[allow(clippy::too_many_arguments)]
async fn start_green<D: DockerHostApi>(
    config: &Config,
    svc: &ServiceConfig,
    generation: u64,
    placements: &[(&crate::config::types::HostConfig, u32)],
    host_ports: &[Option<u16>],
    traefik_network: &str,
    resolved_env: &HashMap<String, String>,
    unique_hosts: &[&str],
    docker_hosts: &HashMap<String, D>,
    created: &CreatedContainers,
) -> Result<()> {
    let pb = output::spinner("Starting new containers...");
    for host_name in unique_hosts {
        docker_hosts[*host_name]
            .ensure_network(traefik_network)
            .await
            .with_context(|| format!("Failed to ensure network on {}", host_name))?;
    }

    for ((host, instance), host_port) in placements.iter().zip(host_ports) {
        let docker = &docker_hosts[host.name.as_str()];
        let container_name =
            labels::container_name(&config.project.name, &svc.name, generation, *instance);

        let container_config = containers::build_container_config(
            &config.project.name,
            svc,
            generation,
            *instance,
            traefik_network,
            resolved_env,
            Some(host.internal_addr()),
            *host_port,
        )?;

        let id = docker
            .create_container(&container_name, container_config)
            .await
            .with_context(|| format!("Failed to create {} on {}", container_name, host.name))?;
        created.push(ContainerRef {
            service: svc.name.clone(),
            host_name: host.name.clone(),
            id: id.clone(),
            name: container_name.clone(),
            host_port: *host_port,
        });
        docker
            .start_container(&id)
            .await
            .with_context(|| format!("Failed to start {} on {}", container_name, host.name))?;
        debug!("Started {} on {}", container_name, host.name);
    }
    pb.finish_and_clear();
    output::success(&format!(
        "Started {} new containers (generation {})",
        placements.len(),
        generation
    ));
    Ok(())
}

/// Stop every running container of `svc` that is not part of `current_generation`.
///
/// Call this only AFTER Traefik has been synced successfully, so traffic already
/// goes to the new containers. Stops run concurrently; every failure is reported.
pub async fn drain_old_containers<D: DockerHostApi>(
    config: &Config,
    svc: &ServiceConfig,
    current_generation: u64,
    docker_hosts: &HashMap<String, D>,
) -> Result<()> {
    let deploy_cfg = Config::deploy_config(svc);
    let state = LiveState::query(docker_hosts, &config.project.name).await?;

    // Safety net: never drain if the generation that should take over is not running.
    let new_running = state
        .running_service_containers(&svc.name)
        .iter()
        .filter(|c| c.generation == current_generation)
        .count();
    if new_running == 0 {
        anyhow::bail!(
            "Refusing to drain '{}': generation {} has no running containers",
            svc.name,
            current_generation
        );
    }

    let old: Vec<ContainerRef> = state
        .running_service_containers(&svc.name)
        .into_iter()
        .filter(|c| c.generation != current_generation)
        .map(ContainerRef::from)
        .collect();

    if old.is_empty() {
        return Ok(());
    }

    let pb = output::spinner(&format!(
        "Draining {} old containers ({}s timeout)...",
        old.len(),
        deploy_cfg.drain_seconds
    ));
    let result = verify::stop_all(docker_hosts, &old, deploy_cfg.drain_seconds as i64).await;
    pb.finish_and_clear();
    result.with_context(|| format!("Failed to drain old containers of '{}'", svc.name))?;
    output::success(&format!("Drained {} old containers", old.len()));
    Ok(())
}

/// Remove stopped generations beyond `rollback_keep`.
///
/// Generations are ranked by when they last served (most recent container start),
/// not by generation number, so the known-good generation that was rolled back to
/// is kept even though newer (bad) generations exist. The current generation and
/// the rollback target are never removed.
pub async fn cleanup_old_generations<D: DockerHostApi>(
    config: &Config,
    svc: &ServiceConfig,
    current_generation: u64,
    docker_hosts: &HashMap<String, D>,
) -> Result<()> {
    let deploy_cfg = Config::deploy_config(svc);
    let state = LiveState::query(docker_hosts, &config.project.name).await?;
    let rollback_target = state.rollback_generation(&svc.name);
    let running_gens: HashSet<u64> = state.running_generations(&svc.name).into_iter().collect();

    // Stopped generations and the latest time any of their containers started.
    let mut last_served: HashMap<u64, String> = HashMap::new();
    for c in state.service_containers(&svc.name) {
        if c.generation == current_generation || running_gens.contains(&c.generation) {
            continue;
        }
        let started = match docker_hosts.get(&c.host_name) {
            Some(docker) => docker
                .inspect_container(&c.id)
                .await
                .ok()
                .and_then(|i| i.state.and_then(|s| s.started_at))
                .map(|s| started_at_key(&s))
                .unwrap_or_default(),
            None => String::new(),
        };
        let entry = last_served.entry(c.generation).or_default();
        if started > *entry {
            *entry = started;
        }
    }

    let mut ranked: Vec<(u64, String)> = last_served.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));

    let mut keep: HashSet<u64> = ranked
        .iter()
        .take(deploy_cfg.rollback_keep as usize)
        .map(|(g, _)| *g)
        .collect();
    keep.insert(current_generation);
    if let Some(target) = rollback_target {
        keep.insert(target);
    }

    let to_remove: Vec<ContainerRef> = state
        .service_containers(&svc.name)
        .into_iter()
        .filter(|c| !keep.contains(&c.generation) && !c.is_running())
        .map(ContainerRef::from)
        .collect();

    if to_remove.is_empty() {
        return Ok(());
    }
    verify::remove_all(docker_hosts, &to_remove).await?;
    output::info(&format!("Cleaned up {} old containers", to_remove.len()));
    Ok(())
}

/// Sortable form of a Docker RFC 3339 timestamp (`2024-05-01T10:00:00.5Z`):
/// the fractional seconds are padded to nanoseconds so string order is time order.
fn started_at_key(ts: &str) -> String {
    let ts = ts.trim().trim_end_matches('Z');
    let (secs, frac) = ts.split_once('.').unwrap_or((ts, ""));
    let frac: String = frac
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(9)
        .collect();
    format!("{}.{:0<9}", secs, frac)
}

/// Generations of `svc` other than `keep` that still have running containers.
pub fn other_running(state: &LiveState, service: &str, keep: u64) -> Vec<ContainerRef> {
    state
        .running_service_containers(service)
        .into_iter()
        .filter(|c: &&KorgiContainer| c.generation != keep)
        .map(ContainerRef::from)
        .collect()
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
                DockerCall::PullImage { image, .. } => Some(image.as_str()),
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

        let generation = deploy_service(&config, svc, None, &hosts, false)
            .await
            .unwrap()
            .unwrap();

        // Drain happens separately (after Traefik sync in real flow)
        drain_old_containers(&config, svc, generation, &hosts)
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

        // Should have force-removed both new containers (cleanup)
        let all_calls: Vec<_> = hosts.values().flat_map(|h| h.get_calls()).collect();
        let remove_count = all_calls
            .iter()
            .filter(|c| matches!(c, DockerCall::RemoveContainer { force: true, .. }))
            .count();
        assert_eq!(
            remove_count, 2,
            "Should remove new containers on health failure"
        );
        assert!(
            hosts
                .values()
                .all(|h| h.containers.lock().unwrap().is_empty())
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
        let mut config = test_config();
        config.services[0].deploy = Some(DeployConfig {
            drain_seconds: 30,
            start_delay: 0,
            rollback_keep: 1,
        });
        let svc = &config.services[0];
        let hosts = mock_hosts();

        // Old stopped gen 1 and running gen 3. After deploying gen 4, gen 3 is the
        // rollback target and fills rollback_keep = 1, so gen 1 is removed.
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

        let generation = deploy_service(&config, svc, None, &hosts, false)
            .await
            .unwrap()
            .unwrap();

        drain_old_containers(&config, svc, generation, &hosts)
            .await
            .unwrap();
        cleanup_old_generations(&config, svc, generation, &hosts)
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

    fn http_health() -> HealthConfig {
        HealthConfig {
            mode: HealthMode::Http,
            path: "/health".to_string(),
            interval: "1s".to_string(),
            timeout: "1s".to_string(),
            retries: 1,
            start_period: None,
        }
    }

    fn running_with_port(
        id: &str,
        generation: u64,
        instance: u32,
        port: &str,
    ) -> bollard::models::ContainerSummary {
        with_label(
            mock_container_summary(
                id,
                &format!("korgi-myapp-api-g{}-{}", generation, instance),
                "myapp",
                "api",
                generation,
                instance,
                "myapp/api:v1",
                ContainerSummaryStateEnum::RUNNING,
                "Up",
            ),
            "korgi.host_port",
            port,
        )
    }

    fn bound_port(host: &MockDockerHost, name: &str) -> Option<u16> {
        containers::host_port_from_bindings(host.created_config(name)?.host_config.as_ref())
    }

    #[tokio::test]
    async fn test_shifted_port_range_is_bound_labelled_and_health_checked() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A local HTTP server stands in for the new container on port P.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = listener.local_addr().unwrap().port();
        assert!(p > 2);
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let server_hits = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                server_hits
                    .lock()
                    .unwrap()
                    .push(req.lines().next().unwrap_or_default().to_string());
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            }
        });

        // One replica, base P-2. Gen 1 holds P-2; an unrelated process holds P-1,
        // so gen 2's default port (P-1) is taken and the allocator shifts to P.
        // The old code health-checked base + (gen-1)*replicas = P-1.
        let mut config = test_config();
        for h in &mut config.hosts {
            h.internal_address = Some("127.0.0.1".to_string());
        }
        config.services[0].replicas = 1;
        config.services[0].ports = Some(PortsConfig {
            container: 8080,
            host: None,
            host_base: Some(p - 2),
        });
        config.services[0].health = Some(http_health());
        config.services[0].routing = Some(RoutingConfig {
            rule: "Host(`api.example.com`)".to_string(),
            entrypoints: vec![],
            tls: false,
            lb_strategy: None,
            sticky: None,
        });
        config.services[0].deploy = Some(DeployConfig {
            drain_seconds: 1,
            start_delay: 0,
            rollback_keep: 2,
        });
        let svc = &config.services[0];
        let hosts = mock_hosts();
        hosts["web1"].add_container(running_with_port("g1-0", 1, 0, &(p - 2).to_string()));
        hosts["web1"].add_container(bollard::models::ContainerSummary {
            id: Some("other".into()),
            state: Some(ContainerSummaryStateEnum::RUNNING),
            ports: Some(vec![bollard::models::PortSummary {
                private_port: 80,
                public_port: Some(p - 1),
                ..Default::default()
            }]),
            ..Default::default()
        });

        let generation = deploy_service(&config, svc, None, &hosts, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(generation, 2);

        assert_eq!(bound_port(&hosts["web1"], "korgi-myapp-api-g2-0"), Some(p));
        let labels = hosts["web1"]
            .created_config("korgi-myapp-api-g2-0")
            .unwrap()
            .labels
            .unwrap();
        assert_eq!(labels.get("korgi.host_port").unwrap(), &p.to_string());
        assert_eq!(hits.lock().unwrap()[0], "GET /health HTTP/1.1");

        // Traefik routes to the same port, read back from the container.
        let state = LiveState::query(&hosts, "myapp").await.unwrap();
        let yaml = crate::orchestrator::traefik_config::generate_dynamic_config(&config, &state);
        assert!(
            yaml.contains(&format!("http://127.0.0.1:{}\"", p)),
            "{}",
            yaml
        );
        assert!(!yaml.contains(&format!("http://127.0.0.1:{}\"", p - 1)));
    }

    #[tokio::test]
    async fn test_start_failure_removes_already_started_containers() {
        let config = test_config();
        let svc = &config.services[0];
        let hosts = mock_hosts();
        // web1 gets instance 0 (ok), web2 fails creating instance 1.
        hosts["web2"].set_create_error("port is already allocated");

        let result = deploy_service(&config, svc, None, &hosts, false).await;
        assert!(result.is_err());
        let removed: Vec<_> = hosts["web1"]
            .get_calls()
            .into_iter()
            .filter(|c| matches!(c, DockerCall::RemoveContainer { .. }))
            .collect();
        assert_eq!(removed.len(), 1, "instance 0 must be removed");
        assert!(hosts["web1"].containers.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_no_health_requires_stable_running_containers() {
        let mut config = test_config();
        config.services[0].deploy = Some(DeployConfig {
            drain_seconds: 1,
            start_delay: 1,
            rollback_keep: 2,
        });
        let svc = &config.services[0];
        let hosts = mock_hosts();
        // Restart count climbs: crash loop.
        hosts["web1"].set_restart_counts(&[0, 1]);
        let result = deploy_service(&config, svc, None, &hosts, false).await;
        assert!(
            result.is_err(),
            "crash-looping container must fail the deploy"
        );

        let hosts = mock_hosts();
        hosts["web2"].set_container_running(false);
        assert!(
            deploy_service(&config, svc, None, &hosts, false)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_ports_of_stopped_rollback_targets_are_reserved() {
        let mut config = test_config();
        config.services[0].ports = Some(PortsConfig {
            container: 8080,
            host: None,
            host_base: Some(9001),
        });
        let svc = &config.services[0];
        let hosts = mock_hosts();
        // Gen 1 stopped (rollback target, recorded 9001/9002), gen 2 running 9003/9004.
        let mut stopped = running_with_port("g1-0", 1, 0, "9001");
        stopped.state = Some(ContainerSummaryStateEnum::EXITED);
        hosts["web1"].add_container(stopped);
        hosts["web1"].add_container(running_with_port("g2-0", 2, 0, "9003"));
        hosts["web2"].add_container(running_with_port("g2-1", 2, 1, "9004"));
        // Gen 3's default block is 9005/9006: free.
        deploy_service(&config, svc, None, &hosts, false)
            .await
            .unwrap();
        assert_eq!(
            bound_port(&hosts["web1"], "korgi-myapp-api-g3-0"),
            Some(9005)
        );

        // Gen 1's stopped container keeps 9001 reserved for when it is rolled back to.
        let used = ports::collect_used_ports_on(&hosts, &["web1", "web2"])
            .await
            .unwrap();
        assert!(
            used.contains(&9001),
            "stopped container's port stays reserved"
        );
        let offset = ports::find_free_port_offset(9001, 2, 1, &used).unwrap();
        assert_ne!(
            offset, 0,
            "a new generation never reuses a rollback target's port"
        );
    }

    #[tokio::test]
    async fn test_drain_refuses_when_new_generation_not_running() {
        let config = test_config();
        let svc = &config.services[0];
        let hosts = mock_hosts();
        hosts["web1"].add_container(running_with_port("g1-0", 1, 0, ""));
        let err = drain_old_containers(&config, svc, 2, &hosts)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Refusing to drain"));
        assert!(
            !hosts["web1"]
                .get_calls()
                .iter()
                .any(|c| matches!(c, DockerCall::StopContainer { .. }))
        );
    }

    #[tokio::test]
    async fn test_drain_stops_concurrently_and_surfaces_errors() {
        let config = test_config();
        let svc = &config.services[0];
        let hosts = mock_hosts();
        hosts["web1"].add_container(running_with_port("g1-0", 1, 0, ""));
        hosts["web2"].add_container(running_with_port("g1-1", 1, 1, ""));
        hosts["web1"].add_container(running_with_port("g2-0", 2, 0, ""));
        hosts["web2"].set_stop_error("g1-1");
        let err = drain_old_containers(&config, svc, 2, &hosts)
            .await
            .unwrap_err();
        assert!(
            format!("{:#}", err).contains("g1-1")
                || format!("{:#}", err).contains("korgi-myapp-api-g1-1")
        );
        // The other stop still happened.
        assert!(
            hosts["web1"]
                .get_calls()
                .contains(&DockerCall::StopContainer {
                    id: "g1-0".into(),
                    timeout: 30
                })
        );
    }

    fn stopped_gen(host: &MockDockerHost, generation: u64, started_at: &str) {
        let id = format!("g{}", generation);
        let mut summary = running_with_port(&id, generation, 0, "");
        summary.state = Some(ContainerSummaryStateEnum::EXITED);
        host.add_container(summary);
        host.set_inspect_response(
            &id,
            bollard::models::ContainerInspectResponse {
                state: Some(bollard::models::ContainerState {
                    running: Some(false),
                    started_at: Some(started_at.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
    }

    #[tokio::test]
    async fn test_cleanup_keeps_known_good_generation_after_rollback() {
        // History: gen 2 good, gen 3 bad -> rolled back to 2 (restarted at 10:05),
        // then gen 4 deployed (gen 2 drained). With rollback_keep = 1 the old
        // number-based cutoff deleted gen 2 and kept the bad gen 3.
        let mut config = test_config();
        config.services[0].deploy = Some(DeployConfig {
            drain_seconds: 1,
            start_delay: 0,
            rollback_keep: 1,
        });
        let svc = &config.services[0];
        let hosts = mock_hosts();
        let web1 = &hosts["web1"];
        stopped_gen(web1, 1, "2024-05-01T09:00:00Z");
        stopped_gen(web1, 2, "2024-05-01T10:05:00.5Z");
        stopped_gen(web1, 3, "2024-05-01T10:00:00.123456789Z");
        web1.add_container(running_with_port("g4", 4, 0, ""));

        cleanup_old_generations(&config, svc, 4, &hosts)
            .await
            .unwrap();

        let removed: Vec<String> = web1
            .get_calls()
            .into_iter()
            .filter_map(|c| match c {
                DockerCall::RemoveContainer { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(removed, vec!["g1".to_string()]);
        // gen 2 kept (most recently serving), gen 3 kept (rollback target), gen 4 current.
    }

    #[tokio::test]
    async fn test_cleanup_never_removes_running_containers() {
        let mut config = test_config();
        config.services[0].deploy = Some(DeployConfig {
            drain_seconds: 1,
            start_delay: 0,
            rollback_keep: 0,
        });
        let svc = &config.services[0];
        let hosts = mock_hosts();
        // A leftover running gen 1 (failed drain) must not be removed by cleanup.
        hosts["web1"].add_container(running_with_port("g1", 1, 0, ""));
        hosts["web1"].add_container(running_with_port("g3", 3, 0, ""));
        stopped_gen(&hosts["web1"], 2, "2024-05-01T09:00:00Z");
        cleanup_old_generations(&config, svc, 3, &hosts)
            .await
            .unwrap();
        let removed: Vec<String> = hosts["web1"]
            .get_calls()
            .into_iter()
            .filter_map(|c| match c {
                DockerCall::RemoveContainer { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        // gen 2 is the rollback target (highest stopped below current) -> kept.
        assert!(removed.is_empty(), "{:?}", removed);
    }

    #[test]
    fn test_started_at_key_orders_fractional_seconds() {
        assert!(
            started_at_key("2024-05-01T10:00:00.51Z") > started_at_key("2024-05-01T10:00:00.5Z")
        );
        assert!(
            started_at_key("2024-05-01T10:00:01Z") > started_at_key("2024-05-01T10:00:00.999Z")
        );
    }
}
