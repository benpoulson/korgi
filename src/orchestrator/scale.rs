use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use tracing::debug;

use crate::cli::output;
use crate::config::interpolate;
use crate::config::types::Config;
use crate::docker::containers;
use crate::docker::labels;
use crate::docker::traits::DockerExecApi;
use crate::orchestrator::placement;
use crate::orchestrator::ports;
use crate::orchestrator::state::LiveState;
use crate::orchestrator::traefik_sync::{sync_traefik_config, sync_traefik_config_excluding};
use crate::orchestrator::verify::{self, ContainerRef, CreatedContainers};

/// Scale a service's current (running) generation to a target replica count.
///
/// Scale-up creates containers with the generation's own image (its
/// `korgi.image` label), on freshly allocated host ports, health-checks them and
/// only then adds them to Traefik. Scale-down removes containers from Traefik
/// first and stops them afterwards.
pub async fn scale_service<D: DockerExecApi>(
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

    let generation_containers = state.generation_containers(service, current_gen);
    let running = generation_containers
        .iter()
        .copied()
        .filter(|c| c.is_running())
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
        // Scale UP: add containers to the running generation, using its image.
        let mut svc_gen = svc.clone();
        svc_gen.image = running[0].image.clone();

        let matching_hosts = config.matching_hosts(svc);
        let all_placements = placement::place_replicas(&matching_hosts.to_vec(), target_count);
        let existing_instances: HashSet<u32> = running.iter().map(|c| c.instance).collect();
        let new_placements: Vec<_> = all_placements
            .into_iter()
            .filter(|(_, instance)| !existing_instances.contains(instance))
            .collect();
        if new_placements.is_empty() {
            anyhow::bail!("No free instance slots to scale '{}' up", service);
        }

        let mut new_hosts: Vec<&str> = new_placements
            .iter()
            .map(|(h, _)| h.name.as_str())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        new_hosts.sort_unstable();
        for host_name in &new_hosts {
            docker_hosts
                .get(*host_name)
                .context(format!("No connection to host {}", host_name))?;
        }

        let host_ports: Vec<Option<u16>> = match svc.ports.as_ref().and_then(|p| p.host_base) {
            Some(base) => {
                let used = ports::collect_used_ports_on(docker_hosts, &new_hosts).await?;
                ports::find_free_ports(base, new_placements.len(), &used)?
                    .into_iter()
                    .map(Some)
                    .collect()
            }
            None => vec![None; new_placements.len()],
        };

        let sys_env = interpolate::system_env();
        let resolved_env = interpolate::interpolate_env(&svc.env, &sys_env)?;

        let created = CreatedContainers::new();
        let pb = output::spinner(&format!(
            "Starting {} new containers...",
            new_placements.len()
        ));
        let started = async {
            for host_name in &new_hosts {
                let docker = &docker_hosts[*host_name];
                verify::ensure_image(docker, config, &svc_gen.image).await?;
                docker.ensure_network(traefik_network).await?;
            }
            for ((host, instance), host_port) in new_placements.iter().zip(&host_ports) {
                let docker = &docker_hosts[host.name.as_str()];
                let name =
                    labels::container_name(&config.project.name, service, current_gen, *instance);

                // A stopped leftover with the same name would block the create.
                if let Some(stale) = generation_containers
                    .iter()
                    .find(|c| c.instance == *instance && !c.is_running())
                    && let Some(d) = docker_hosts.get(&stale.host_name)
                {
                    d.remove_container(&stale.id, true).await?;
                }

                let container_config = containers::build_container_config(
                    &config.project.name,
                    &svc_gen,
                    current_gen,
                    *instance,
                    traefik_network,
                    &resolved_env,
                    Some(host.internal_addr()),
                    *host_port,
                )?;

                let id = docker
                    .create_container(&name, container_config)
                    .await
                    .with_context(|| format!("Failed to create {} on {}", name, host.name))?;
                created.push(ContainerRef {
                    service: service.to_string(),
                    host_name: host.name.clone(),
                    id: id.clone(),
                    name: name.clone(),
                    host_port: *host_port,
                });
                docker
                    .start_container(&id)
                    .await
                    .with_context(|| format!("Failed to start {} on {}", name, host.name))?;
                debug!("Started {} on {}", name, host.name);
            }
            verify::verify_healthy(config, &svc_gen, &created.snapshot(), docker_hosts)
                .await
                .context("new containers failed their health check")
        }
        .await;
        pb.finish_and_clear();

        if let Err(e) = started {
            verify::discard_created(docker_hosts, &created.take_all()).await;
            return Err(e.context(format!("Scale-up of '{}' aborted", service)));
        }
        output::success(&format!(
            "Added {} healthy containers",
            new_placements.len()
        ));

        sync_traefik_config(config, docker_hosts)
            .await
            .context("Traefik sync after scale-up failed")?;
    } else {
        // Scale DOWN: take containers out of Traefik first, then stop them.
        let remove_count = (current_count - target_count) as usize;
        let mut to_remove = running.clone();
        to_remove.sort_by_key(|c| std::cmp::Reverse(c.instance));
        let to_remove: Vec<ContainerRef> = to_remove[..remove_count]
            .iter()
            .map(|c| ContainerRef::from(*c))
            .collect();
        let exclude: HashSet<String> = to_remove.iter().map(|c| c.id.clone()).collect();

        sync_traefik_config_excluding(config, docker_hosts, &exclude)
            .await
            .context("Traefik sync failed; no containers were stopped")?;

        let deploy_cfg = Config::deploy_config(svc);
        let pb = output::spinner(&format!("Stopping {} containers...", remove_count));
        let stopped =
            verify::stop_all(docker_hosts, &to_remove, deploy_cfg.drain_seconds as i64).await;
        let removed = match &stopped {
            Ok(()) => verify::remove_all(docker_hosts, &to_remove).await,
            Err(_) => Ok(()),
        };
        pb.finish_and_clear();

        let final_sync = sync_traefik_config(config, docker_hosts).await;
        stopped.context("Failed to stop containers")?;
        removed.context("Failed to remove containers")?;
        final_sync.context("Final Traefik sync after scale-down failed")?;
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

    fn mock_hosts_with_running(generation: u64) -> HashMap<String, MockDockerHost> {
        let mut hosts = HashMap::new();
        let web1 = MockDockerHost::new("web1");
        web1.add_container(mock_container_summary(
            "c1",
            "korgi-myapp-api-g1-0",
            "myapp",
            "api",
            generation,
            0,
            "myapp/api:v1",
            ContainerSummaryStateEnum::RUNNING,
            "Up 5 minutes",
        ));
        let web2 = MockDockerHost::new("web2");
        web2.add_container(mock_container_summary(
            "c2",
            "korgi-myapp-api-g1-1",
            "myapp",
            "api",
            generation,
            1,
            "myapp/api:v1",
            ContainerSummaryStateEnum::RUNNING,
            "Up 5 minutes",
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

        let all_calls: Vec<_> = hosts.values().flat_map(|h| h.get_calls()).collect();
        // Should create 2 new containers (scaling from 2 to 4)
        let create_count = all_calls
            .iter()
            .filter(|c| matches!(c, DockerCall::CreateContainer { .. }))
            .count();
        assert_eq!(create_count, 2, "Should create 2 new containers");

        let start_count = all_calls
            .iter()
            .filter(|c| matches!(c, DockerCall::StartContainer { .. }))
            .count();
        assert_eq!(start_count, 2, "Should start 2 new containers");
    }

    #[tokio::test]
    async fn test_scale_down_removes_containers() {
        let config = test_config();
        let hosts = mock_hosts_with_running(1);

        scale_service(&config, "api", 1, &hosts).await.unwrap();

        let all_calls: Vec<_> = hosts.values().flat_map(|h| h.get_calls()).collect();
        // Should stop 1 container (scaling from 2 to 1)
        let stop_count = all_calls
            .iter()
            .filter(|c| matches!(c, DockerCall::StopContainer { .. }))
            .count();
        assert_eq!(stop_count, 1, "Should stop 1 container");

        // Should remove 1 container
        let remove_count = all_calls
            .iter()
            .filter(|c| matches!(c, DockerCall::RemoveContainer { .. }))
            .count();
        assert_eq!(remove_count, 1, "Should remove 1 container");
    }

    #[tokio::test]
    async fn test_scale_same_count_noop() {
        let config = test_config();
        let hosts = mock_hosts_with_running(1);

        scale_service(&config, "api", 2, &hosts).await.unwrap();

        let all_calls: Vec<_> = hosts.values().flat_map(|h| h.get_calls()).collect();
        // Should not create or stop anything
        assert!(
            !all_calls
                .iter()
                .any(|c| matches!(c, DockerCall::CreateContainer { .. }))
        );
        assert!(
            !all_calls
                .iter()
                .any(|c| matches!(c, DockerCall::StopContainer { .. }))
        );
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
                &format!("c{}", i),
                &format!("korgi-myapp-api-g1-{}", i),
                "myapp",
                "api",
                1,
                i,
                "myapp/api:v1",
                ContainerSummaryStateEnum::RUNNING,
                "Up",
            ));
        }
        hosts.insert("web1".to_string(), web1);
        hosts.insert("web2".to_string(), MockDockerHost::new("web2"));

        scale_service(&config, "api", 1, &hosts).await.unwrap();

        // Should remove instance 2 and 1 (highest first), keeping instance 0
        let web1_calls = hosts.get("web1").unwrap().get_calls();
        let stopped: Vec<_> = web1_calls
            .iter()
            .filter_map(|c| match c {
                DockerCall::StopContainer { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(stopped.len(), 2);
        assert!(
            stopped.contains(&"c2"),
            "Should stop highest instance first"
        );
        assert!(
            stopped.contains(&"c1"),
            "Should stop second highest instance"
        );
    }

    #[tokio::test]
    async fn test_scale_to_zero_removes_all() {
        let config = test_config();
        let hosts = mock_hosts_with_running(1);

        scale_service(&config, "api", 0, &hosts).await.unwrap();

        let all_calls: Vec<_> = hosts.values().flat_map(|h| h.get_calls()).collect();
        // Should stop both containers
        let stop_count = all_calls
            .iter()
            .filter(|c| matches!(c, DockerCall::StopContainer { .. }))
            .count();
        assert_eq!(
            stop_count, 2,
            "Should stop all 2 containers when scaling to 0"
        );

        // Should remove both containers
        let remove_count = all_calls
            .iter()
            .filter(|c| matches!(c, DockerCall::RemoveContainer { .. }))
            .count();
        assert_eq!(
            remove_count, 2,
            "Should remove all 2 containers when scaling to 0"
        );
    }

    fn traefik_config() -> Config {
        let mut config = test_config();
        config.hosts.push({
            let mut h = HostConfig::test_host("lb", "10.0.0.100");
            h.role = HostRole::Lb;
            h
        });
        config.traefik.as_mut().unwrap().hosts = vec!["lb".to_string()];
        let svc = &mut config.services[0];
        // Config now points at v9, but the running generation was built from v1.
        svc.image = "myapp/api:v9".to_string();
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

    fn labelled_hosts(log: &SharedLog) -> HashMap<String, MockDockerHost> {
        let mut hosts = HashMap::new();
        for (name, id, instance, port) in [("web1", "c0", 0, "9003"), ("web2", "c1", 1, "9004")] {
            let h = MockDockerHost::new(name).with_shared_log(log);
            h.add_container(with_label(
                mock_container_summary(
                    id,
                    &format!("korgi-myapp-api-g2-{}", instance),
                    "myapp",
                    "api",
                    2,
                    instance,
                    "myapp/api:v1",
                    ContainerSummaryStateEnum::RUNNING,
                    "Up",
                ),
                "korgi.host_port",
                port,
            ));
            hosts.insert(name.to_string(), h);
        }
        hosts.insert(
            "lb".to_string(),
            MockDockerHost::new("lb").with_shared_log(log),
        );
        hosts
    }

    #[tokio::test]
    async fn test_scale_up_uses_generation_image_free_ports_and_routes_after_health() {
        let config = traefik_config();
        let log = SharedLog::default();
        let hosts = labelled_hosts(&log);

        scale_service(&config, "api", 3, &hosts).await.unwrap();

        let cfg = hosts["web1"]
            .created_config("korgi-myapp-api-g2-2")
            .unwrap();
        assert_eq!(
            cfg.image.as_deref(),
            Some("myapp/api:v1"),
            "generation's image, not config's"
        );
        let port = crate::docker::containers::host_port_from_bindings(cfg.host_config.as_ref());
        assert_eq!(port, Some(9001), "first free port, not a recomputed one");
        assert_eq!(cfg.labels.unwrap().get("korgi.host_port").unwrap(), "9001");

        let entries = log.lock().unwrap().clone();
        let started = entries
            .iter()
            .position(|(_, c)| matches!(c, DockerCall::StartContainer { .. }))
            .unwrap();
        let inspected = entries
            .iter()
            .rposition(|(h, c)| h == "web1" && matches!(c, DockerCall::InspectContainer { id } if id.starts_with("web1-container")))
            .expect("new container health-checked");
        let synced = entries
            .iter()
            .position(
                |(_, c)| matches!(c, DockerCall::Exec { script, .. } if script.contains("mv -f")),
            )
            .unwrap();
        assert!(started < inspected && inspected < synced);
        let yaml = hosts["lb"]
            .file("korgi-traefik", "/etc/korgi/dynamic.yml")
            .unwrap();
        assert!(yaml.contains("http://10.0.0.1:9001\""));
        assert!(yaml.contains("http://10.0.0.1:9003\""));
    }

    #[tokio::test]
    async fn test_scale_up_health_failure_removes_new_containers() {
        let config = traefik_config();
        let log = SharedLog::default();
        let hosts = labelled_hosts(&log);
        hosts["web1"].set_container_running(false);
        assert!(scale_service(&config, "api", 3, &hosts).await.is_err());
        assert!(
            hosts["web1"]
                .created_config("korgi-myapp-api-g2-2")
                .is_some()
        );
        assert!(
            hosts["web1"]
                .get_calls()
                .iter()
                .any(|c| matches!(c, DockerCall::RemoveContainer { id, .. } if id.starts_with("web1-container")))
        );
        assert!(
            !log.lock()
                .unwrap()
                .iter()
                .any(|(_, c)| matches!(c, DockerCall::Exec { .. })),
            "never routed"
        );
    }

    #[tokio::test]
    async fn test_scale_down_syncs_before_stopping() {
        let config = traefik_config();
        let log = SharedLog::default();
        let hosts = labelled_hosts(&log);

        scale_service(&config, "api", 1, &hosts).await.unwrap();

        let entries = log.lock().unwrap().clone();
        let first_write = entries
            .iter()
            .position(
                |(_, c)| matches!(c, DockerCall::Exec { script, .. } if script.contains("mv -f")),
            )
            .unwrap();
        let stop = entries
            .iter()
            .position(|(_, c)| matches!(c, DockerCall::StopContainer { id, .. } if id == "c1"))
            .unwrap();
        assert!(first_write < stop, "Traefik updated before stopping");
        // The config written before the stop already excluded c1 (port 9004).
        let writes: Vec<_> = entries
            .iter()
            .filter(
                |(_, c)| matches!(c, DockerCall::Exec { script, .. } if script.contains("mv -f")),
            )
            .collect();
        assert_eq!(writes.len(), 2);
        if let DockerCall::Exec { script, .. } = &writes[0].1 {
            assert!(!script.contains("9004"));
            assert!(script.contains("9003"));
        }
    }

    #[tokio::test]
    async fn test_scale_down_sync_failure_stops_nothing() {
        let config = traefik_config();
        let log = SharedLog::default();
        let hosts = labelled_hosts(&log);
        hosts["lb"].set_exec_error("exit 1");
        assert!(scale_service(&config, "api", 1, &hosts).await.is_err());
        assert!(
            !log.lock()
                .unwrap()
                .iter()
                .any(|(_, c)| matches!(c, DockerCall::StopContainer { .. }))
        );
    }

    #[tokio::test]
    async fn test_scale_after_rollback_targets_running_generation() {
        let config = test_config();
        let mut hosts = HashMap::new();
        let web1 = MockDockerHost::new("web1");
        // gen 3 stopped (rolled back from), gen 2 running.
        web1.add_container(mock_container_summary(
            "g3",
            "korgi-myapp-api-g3-0",
            "myapp",
            "api",
            3,
            0,
            "myapp/api:v3",
            ContainerSummaryStateEnum::EXITED,
            "Exited",
        ));
        web1.add_container(mock_container_summary(
            "g2",
            "korgi-myapp-api-g2-0",
            "myapp",
            "api",
            2,
            0,
            "myapp/api:v2",
            ContainerSummaryStateEnum::RUNNING,
            "Up",
        ));
        hosts.insert("web1".to_string(), web1);
        hosts.insert("web2".to_string(), MockDockerHost::new("web2"));

        scale_service(&config, "api", 2, &hosts).await.unwrap();
        let created = hosts["web2"].created_config("korgi-myapp-api-g2-1");
        assert!(
            created.is_some(),
            "scaled the running generation 2, not stopped gen 3"
        );
        assert_eq!(created.unwrap().image.as_deref(), Some("myapp/api:v2"));
    }
}
