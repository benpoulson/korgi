use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::watch;

use crate::cli::output;
use crate::config::types::{Config, ServiceConfig};
use crate::docker::host::DockerHost;
use crate::docker::traits::DockerExecApi;
use crate::orchestrator::deploy::{
    cleanup_old_generations, deploy_service_tracked, drain_old_containers,
};
use crate::orchestrator::traefik_sync::sync_traefik_config;
use crate::orchestrator::verify::{self, CreatedContainers};

/// Interrupt (Ctrl-C) state shared between the signal listener and the deploy.
pub struct Interrupt {
    tx: watch::Sender<bool>,
    routing_started: AtomicBool,
}

impl Default for Interrupt {
    fn default() -> Self {
        Self::new()
    }
}

impl Interrupt {
    pub fn new() -> Self {
        Self {
            tx: watch::channel(false).0,
            routing_started: AtomicBool::new(false),
        }
    }

    pub fn trigger(&self) {
        self.tx.send_replace(true);
    }

    pub fn is_triggered(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolves once [`Interrupt::trigger`] has been called.
    pub async fn wait(&self) {
        let mut rx = self.tx.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }

    pub fn mark_routing_started(&self) {
        self.routing_started.store(true, Ordering::SeqCst);
    }

    pub fn routing_started(&self) -> bool {
        self.routing_started.load(Ordering::SeqCst)
    }
}

pub async fn run(
    config: &Config,
    service_filter: Option<&str>,
    image_override: Option<&str>,
    dry_run: bool,
    auto_yes: bool,
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    let services: Vec<_> = if let Some(name) = service_filter {
        let svc = config
            .services
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| anyhow::anyhow!("Service '{}' not found in config", name))?;
        vec![svc]
    } else if config.services.len() > 1 && !dry_run {
        // Interactive multi-select when multiple services and no explicit filter
        let names: Vec<&str> = config.services.iter().map(|s| s.name.as_str()).collect();
        let selected = output::multi_select("Select services to deploy:", &names, auto_yes);
        if selected.is_empty() {
            output::info("No services selected");
            return Ok(());
        }
        selected.iter().map(|&i| &config.services[i]).collect()
    } else {
        config.services.iter().collect()
    };

    if dry_run {
        output::info("Dry run mode -- no changes will be made");
    }

    let interrupt = Arc::new(Interrupt::new());
    let listener = {
        let interrupt = interrupt.clone();
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                if interrupt.routing_started() {
                    output::warn(
                        "Interrupt received after the routing switch; finishing Traefik sync and drain so routing stays consistent...",
                    );
                } else {
                    output::warn("Interrupt received; removing new containers...");
                }
                interrupt.trigger();
            }
        })
    };

    let result = deploy_services(
        config,
        &services,
        image_override,
        dry_run,
        docker_hosts,
        &interrupt,
    )
    .await;
    listener.abort();
    result
}

/// Deploy `services` with the zero-downtime ordering:
///
/// A. start + health-check new containers of EVERY service (nothing routed yet;
///    any failure or interrupt removes all new containers and stops here)
/// B. sync Traefik on every Traefik host (must succeed everywhere, else stop
///    here with old and new both running -- nothing is drained)
/// C. drain old generations
/// D. sync Traefik again (always attempted once B succeeded)
/// E. remove generations beyond `rollback_keep`
pub async fn deploy_services<D: DockerExecApi>(
    config: &Config,
    services: &[&ServiceConfig],
    image_override: Option<&str>,
    dry_run: bool,
    docker_hosts: &HashMap<String, D>,
    interrupt: &Interrupt,
) -> Result<()> {
    // Phase A: start + health check everything before routing anything.
    let created = CreatedContainers::new();
    let phase_a = async {
        let mut deployed: Vec<(&ServiceConfig, u64)> = Vec::new();
        for svc in services {
            if let Some(generation) =
                deploy_service_tracked(config, svc, image_override, docker_hosts, dry_run, &created)
                    .await?
            {
                deployed.push((*svc, generation));
            }
        }
        Ok::<_, anyhow::Error>(deployed)
    };

    let deployed = tokio::select! {
        biased;
        _ = interrupt.wait() => {
            verify::discard_created(docker_hosts, &created.take_all()).await;
            anyhow::bail!(
                "Deploy interrupted before the routing switch; new containers removed, existing containers untouched"
            );
        }
        result = phase_a => match result {
            Ok(deployed) => deployed,
            Err(e) => {
                // A failed service already removed its own containers; remove
                // the healthy-but-unrouted containers of the others too, so a
                // multi-service deploy is all-or-nothing up to the routing switch.
                let others = created.take_all();
                if !others.is_empty() {
                    output::info("Removing new containers of the other services (nothing was routed yet)");
                    verify::discard_created(docker_hosts, &others).await;
                }
                return Err(e);
            }
        }
    };
    if interrupt.is_triggered() {
        verify::discard_created(docker_hosts, &created.take_all()).await;
        anyhow::bail!(
            "Deploy interrupted before the routing switch; new containers removed, existing containers untouched"
        );
    }

    if dry_run || deployed.is_empty() {
        return Ok(());
    }

    // Phase B: route traffic to old + new. From here on an interrupt no longer
    // aborts: the remaining steps keep routing and containers consistent.
    interrupt.mark_routing_started();
    let _ = created.take_all();
    if let Err(e) = sync_traefik_config(config, docker_hosts).await {
        output::error(
            "Traefik sync failed: old containers were NOT drained. Old and new generations are both running; \
             fix Traefik and re-run 'korgi deploy' (or 'korgi traefik deploy').",
        );
        return Err(e.context("Traefik sync failed before drain"));
    }

    // Phase C: drain old generations (all services; keep going on errors).
    let mut errors: Vec<String> = Vec::new();
    for (svc, generation) in &deployed {
        if let Err(e) = drain_old_containers(config, svc, *generation, docker_hosts).await {
            output::error(&format!("{:#}", e));
            errors.push(format!("drain '{}': {:#}", svc.name, e));
        }
    }

    // Phase D: always re-sync so stopped containers leave the routing table.
    if let Err(e) = sync_traefik_config(config, docker_hosts).await {
        output::error(&format!("Final Traefik sync failed: {:#}", e));
        errors.push(format!("final Traefik sync: {:#}", e));
    }

    // Phase E: cleanup (harmless if it fails).
    for (svc, generation) in &deployed {
        if let Err(e) = cleanup_old_generations(config, svc, *generation, docker_hosts).await {
            output::warn(&format!("Cleanup of '{}' incomplete: {:#}", svc.name, e));
        }
    }

    if !errors.is_empty() {
        anyhow::bail!(
            "Deploy finished with errors (new generation is serving): {}",
            errors.join("; ")
        );
    }

    if deployed.len() > 1 {
        output::success(&format!("All {} services deployed", deployed.len()));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::*;
    use crate::docker::mock::tests::*;
    use crate::orchestrator::traefik_sync::{CONFIG_FILE, TRAEFIK_CONTAINER_NAME};
    use bollard::models::{ContainerSummaryStateEnum, HealthStatusEnum};
    use std::time::Duration;

    fn config() -> Config {
        Config {
            project: ProjectConfig {
                name: "myapp".to_string(),
                secrets: None,
            },
            registries: vec![],
            hosts: vec![
                {
                    let mut h = HostConfig::test_host("lb", "10.0.0.100");
                    h.role = HostRole::Lb;
                    h
                },
                {
                    let mut h = HostConfig::test_host("web1", "10.0.0.1");
                    h.labels = vec!["web".to_string()];
                    h
                },
            ],
            traefik: Some(TraefikConfig {
                image: "traefik:v3.2".to_string(),
                hosts: vec![],
                entrypoints: HashMap::new(),
                network: "korgi-traefik".to_string(),
                acme: None,
                passthroughs: Vec::new(),
            }),
            services: ["api", "web"]
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let mut svc = ServiceConfig::test_service(name, &format!("myapp/{}:v2", name));
                    svc.replicas = 1;
                    svc.placement_labels = vec!["web".to_string()];
                    svc.routing = Some(RoutingConfig {
                        rule: format!("Host(`{}.example.com`)", name),
                        entrypoints: vec![],
                        tls: false,
                        lb_strategy: None,
                        sticky: None,
                    });
                    svc.ports = Some(PortsConfig {
                        container: 8080,
                        host: None,
                        host_base: Some(9001 + 100 * i as u16),
                    });
                    svc.deploy = Some(DeployConfig {
                        drain_seconds: 1,
                        start_delay: 0,
                        rollback_keep: 2,
                    });
                    svc
                })
                .collect(),
        }
    }

    fn hosts(log: &SharedLog) -> HashMap<String, MockDockerHost> {
        let mut hosts = HashMap::new();
        hosts.insert(
            "lb".to_string(),
            MockDockerHost::new("lb").with_shared_log(log),
        );
        let web1 = MockDockerHost::new("web1").with_shared_log(log);
        web1.add_container(with_label(
            mock_container_summary(
                "api-old",
                "korgi-myapp-api-g1-0",
                "myapp",
                "api",
                1,
                0,
                "myapp/api:v1",
                ContainerSummaryStateEnum::RUNNING,
                "Up",
            ),
            "korgi.host_port",
            "9001",
        ));
        web1.add_container(with_label(
            mock_container_summary(
                "web-old",
                "korgi-myapp-web-g1-0",
                "myapp",
                "web",
                1,
                0,
                "myapp/web:v1",
                ContainerSummaryStateEnum::RUNNING,
                "Up",
            ),
            "korgi.host_port",
            "9101",
        ));
        hosts.insert("web1".to_string(), web1);
        hosts
    }

    fn stops(log: &SharedLog) -> Vec<String> {
        log.lock()
            .unwrap()
            .iter()
            .filter_map(|(_, c)| match c {
                DockerCall::StopContainer { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    fn removed(log: &SharedLog) -> Vec<String> {
        log.lock()
            .unwrap()
            .iter()
            .filter_map(|(_, c)| match c {
                DockerCall::RemoveContainer { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    fn position(log: &SharedLog, pred: impl Fn(&(String, DockerCall)) -> bool) -> Option<usize> {
        log.lock().unwrap().iter().position(pred)
    }

    fn is_write(entry: &(String, DockerCall)) -> bool {
        matches!(&entry.1, DockerCall::Exec { script, .. } if script.contains("mv -f"))
    }

    #[tokio::test]
    async fn test_full_deploy_syncs_before_drain_and_resyncs_after() {
        let config = config();
        let log = SharedLog::default();
        let hosts = hosts(&log);
        let services: Vec<&ServiceConfig> = config.services.iter().collect();

        deploy_services(&config, &services, None, false, &hosts, &Interrupt::new())
            .await
            .unwrap();

        let first_write = position(&log, is_write).expect("synced");
        let first_stop =
            position(&log, |e| matches!(e.1, DockerCall::StopContainer { .. })).expect("drained");
        assert!(first_write < first_stop, "Traefik synced before any drain");
        let writes = log.lock().unwrap().iter().filter(|e| is_write(e)).count();
        assert_eq!(writes, 2, "sync before drain and after drain");

        let mut stopped = stops(&log);
        stopped.sort();
        assert_eq!(stopped, vec!["api-old", "web-old"]);

        // Final config routes only the new containers, on the ports they bound.
        let yaml = hosts["lb"]
            .file(TRAEFIK_CONTAINER_NAME, CONFIG_FILE)
            .unwrap();
        assert!(!yaml.contains(":9001\""), "old api port gone: {}", yaml);
        assert!(yaml.contains("http://10.0.0.1:9002"), "{}", yaml);
        assert!(yaml.contains("http://10.0.0.1:9102"), "{}", yaml);
    }

    #[tokio::test]
    async fn test_sync_failure_aborts_before_drain() {
        let config = config();
        let log = SharedLog::default();
        let hosts = hosts(&log);
        hosts["lb"].set_inspect_error(TRAEFIK_CONTAINER_NAME);
        let services: Vec<&ServiceConfig> = config.services.iter().collect();

        let err = deploy_services(&config, &services, None, false, &hosts, &Interrupt::new())
            .await
            .unwrap_err();
        assert!(format!("{:#}", err).contains("Traefik"), "{:#}", err);
        assert!(stops(&log).is_empty(), "nothing drained when sync fails");
        assert!(
            removed(&log).is_empty(),
            "new containers stay up, routed or not"
        );
    }

    #[tokio::test]
    async fn test_sync_read_back_mismatch_aborts_before_drain() {
        let config = config();
        let log = SharedLog::default();
        let hosts = hosts(&log);
        hosts["lb"].set_exec_read_override("# stale\n");
        let services: Vec<&ServiceConfig> = config.services.iter().collect();

        assert!(
            deploy_services(&config, &services, None, false, &hosts, &Interrupt::new())
                .await
                .is_err()
        );
        assert!(stops(&log).is_empty());
    }

    #[tokio::test]
    async fn test_second_service_failure_removes_all_new_containers() {
        let config = config();
        let log = SharedLog::default();
        let hosts = hosts(&log);
        // api (first) creates fine; web (second) fails to create.
        hosts["web1"].set_create_fail_after(1);
        let services: Vec<&ServiceConfig> = config.services.iter().collect();

        assert!(
            deploy_services(&config, &services, None, false, &hosts, &Interrupt::new())
                .await
                .is_err()
        );
        assert!(stops(&log).is_empty(), "old generations untouched");
        assert!(
            position(&log, is_write).is_none(),
            "nothing routed when any service fails before the switch"
        );
        assert_eq!(removed(&log), vec!["web1-container-1".to_string()]);
    }

    #[tokio::test]
    async fn test_drain_error_still_runs_final_sync() {
        let config = config();
        let log = SharedLog::default();
        let hosts = hosts(&log);
        hosts["web1"].set_stop_error("api-old");
        let services: Vec<&ServiceConfig> = config.services.iter().collect();

        let err = deploy_services(&config, &services, None, false, &hosts, &Interrupt::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("drain"));
        let writes = log.lock().unwrap().iter().filter(|e| is_write(e)).count();
        assert_eq!(writes, 2, "final sync attempted despite drain failure");
        assert!(
            stops(&log).contains(&"web-old".to_string()),
            "other service drained"
        );
    }

    #[tokio::test]
    async fn test_interrupt_before_routing_removes_new_containers() {
        let mut config = config();
        // Health checks never finish, so the interrupt lands during phase A.
        for svc in &mut config.services {
            svc.health = Some(HealthConfig {
                mode: HealthMode::Docker,
                path: "/health".into(),
                interval: "1s".into(),
                timeout: "1s".into(),
                retries: 100,
                start_period: None,
            });
        }
        let log = SharedLog::default();
        let hosts = hosts(&log);
        hosts["web1"].set_health_status(Some(HealthStatusEnum::STARTING));
        let services: Vec<&ServiceConfig> = config.services.iter().take(1).collect();
        let interrupt = Interrupt::new();

        let (result, _) = tokio::join!(
            deploy_services(&config, &services, None, false, &hosts, &interrupt),
            async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                interrupt.trigger();
            }
        );
        let err = result.unwrap_err();
        assert!(err.to_string().contains("interrupted"), "{}", err);
        assert_eq!(removed(&log), vec!["web1-container-1".to_string()]);
        assert!(stops(&log).is_empty());
        assert!(position(&log, is_write).is_none());
    }
}
