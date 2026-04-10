use anyhow::{Context, Result};
use bollard::models::{ContainerCreateBody, ContainerInspectResponse, HealthConfig, RestartPolicy};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::config::interpolate;
use crate::config::types::{Config, HostConfig, ServiceConfig};
use crate::docker::containers::{self, KorgiContainer};
use crate::docker::traits::DockerHostApi;
use crate::orchestrator::placement;
use crate::orchestrator::state::LiveState;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceDiffAction {
    Create,
    Update,
    Replace,
    Noop,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PlacementPlan {
    pub instance: u32,
    pub host: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FieldChange {
    pub field: String,
    pub before: Value,
    pub after: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceDiff {
    pub service: String,
    pub action: ServiceDiffAction,
    pub current_running_generation: Option<u64>,
    pub planned_generation: u64,
    pub current_placements: Vec<PlacementPlan>,
    pub planned_placements: Vec<PlacementPlan>,
    pub field_changes: Vec<FieldChange>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffSummary {
    pub changed_services: usize,
    pub unchanged_services: usize,
    pub total_planned_containers: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    pub project: String,
    pub service_count: usize,
    pub summary: DiffSummary,
    pub services: Vec<ServiceDiff>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct NormalizedPort {
    instance: u32,
    host: String,
    bind_ip: String,
    container_port: u16,
    host_port: Option<u16>,
    host_base: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct NormalizedHealthcheck {
    test: Vec<String>,
    interval: Option<i64>,
    timeout: Option<i64>,
    retries: Option<i64>,
    start_period: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct NormalizedResources {
    memory: Option<i64>,
    nano_cpus: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct NormalizedInstanceSpec {
    instance: u32,
    host: String,
    image: Option<String>,
    env: BTreeMap<String, String>,
    labels: BTreeMap<String, String>,
    volumes: Vec<String>,
    restart_policy: Option<String>,
    healthcheck: Option<NormalizedHealthcheck>,
    resources: NormalizedResources,
    cmd: Vec<String>,
    entrypoint: Vec<String>,
    ports: Vec<NormalizedPort>,
}

pub async fn diff_services<D: DockerHostApi>(
    config: &Config,
    service_filter: Option<&str>,
    image_override: Option<&str>,
    docker_hosts: &HashMap<String, D>,
) -> Result<DiffReport> {
    let services = select_services(config, service_filter)?;
    let state = LiveState::query(docker_hosts, &config.project.name).await?;

    let mut diffs = Vec::new();
    for svc in services {
        let image = image_override.unwrap_or(&svc.image);
        diffs.push(diff_service(config, svc, image, docker_hosts, &state).await?);
    }

    let changed_services = diffs
        .iter()
        .filter(|diff| diff.action != ServiceDiffAction::Noop)
        .count();
    let unchanged_services = diffs.len().saturating_sub(changed_services);
    let total_planned_containers = diffs.iter().map(|diff| diff.planned_placements.len()).sum();

    Ok(DiffReport {
        project: config.project.name.clone(),
        service_count: diffs.len(),
        summary: DiffSummary {
            changed_services,
            unchanged_services,
            total_planned_containers,
        },
        services: diffs,
    })
}

fn select_services<'a>(
    config: &'a Config,
    service_filter: Option<&str>,
) -> Result<Vec<&'a ServiceConfig>> {
    if let Some(name) = service_filter {
        let svc = config
            .services
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| anyhow::anyhow!("Service '{}' not found in config", name))?;
        Ok(vec![svc])
    } else {
        Ok(config.services.iter().collect())
    }
}

async fn diff_service<D: DockerHostApi>(
    config: &Config,
    svc: &ServiceConfig,
    image: &str,
    docker_hosts: &HashMap<String, D>,
    state: &LiveState,
) -> Result<ServiceDiff> {
    let running_generation = state
        .running_service_containers(&svc.name)
        .iter()
        .map(|container| container.generation)
        .max();
    let planned_generation = state.next_generation(&svc.name);

    let current_running: Vec<&KorgiContainer> = running_generation
        .map(|generation| state.generation_containers(&svc.name, generation))
        .unwrap_or_default()
        .into_iter()
        .filter(|container| container.state == "running")
        .collect();

    let matching_hosts = config.matching_hosts(svc);
    if matching_hosts.is_empty() {
        anyhow::bail!("No hosts match placement labels for service '{}'", svc.name);
    }
    let placements = placement::place_replicas(&matching_hosts, svc.replicas);
    let planned_placements = placements
        .iter()
        .map(|(host, instance)| PlacementPlan {
            instance: *instance,
            host: host.name.clone(),
        })
        .collect::<Vec<_>>();

    let mut current_placements = current_running
        .iter()
        .map(|container| PlacementPlan {
            instance: container.instance,
            host: container.host_name.clone(),
        })
        .collect::<Vec<_>>();
    current_placements.sort_by_key(|placement| placement.instance);

    if current_running.is_empty() {
        return Ok(ServiceDiff {
            service: svc.name.clone(),
            action: ServiceDiffAction::Create,
            current_running_generation: None,
            planned_generation,
            current_placements,
            planned_placements,
            field_changes: vec![FieldChange {
                field: "service".to_string(),
                before: json!("absent"),
                after: json!("create"),
            }],
        });
    }

    let normalized_current = normalize_live_instances(svc, docker_hosts, &current_running)?;
    let normalized_desired = normalize_desired_instances(
        config,
        svc,
        image,
        planned_generation,
        &placements,
        docker_hosts,
    )
    .await?;

    let field_changes = compute_field_changes(
        svc,
        image,
        running_generation,
        planned_generation,
        &normalized_current,
        &normalized_desired,
        &current_placements,
        &planned_placements,
    );

    let action = if field_changes.is_empty() {
        ServiceDiffAction::Noop
    } else {
        ServiceDiffAction::Replace
    };

    Ok(ServiceDiff {
        service: svc.name.clone(),
        action,
        current_running_generation: running_generation,
        planned_generation,
        current_placements,
        planned_placements,
        field_changes,
    })
}

async fn normalize_desired_instances<D: DockerHostApi>(
    config: &Config,
    svc: &ServiceConfig,
    image: &str,
    planned_generation: u64,
    placements: &[(&HostConfig, u32)],
    docker_hosts: &HashMap<String, D>,
) -> Result<Vec<NormalizedInstanceSpec>> {
    let sys_env = interpolate::system_env();
    let resolved_env = interpolate::interpolate_env(&svc.env, &sys_env)?;
    let traefik_network = config
        .traefik
        .as_ref()
        .map(|t| t.network.as_str())
        .unwrap_or("korgi-default");

    let mut svc_for_desired = svc.clone();
    svc_for_desired.image = image.to_string();

    let port_offset =
        compute_port_offset(svc, planned_generation, placements, docker_hosts).await?;

    let mut instances = Vec::new();
    for (host, instance) in placements {
        let config = containers::build_container_config(
            &config.project.name,
            &svc_for_desired,
            planned_generation,
            *instance,
            traefik_network,
            &resolved_env,
            Some(host.internal_addr()),
            port_offset,
        );
        instances.push(normalize_desired_instance(svc, host, *instance, config));
    }
    instances.sort_by_key(|instance| instance.instance);
    Ok(instances)
}

async fn compute_port_offset<D: DockerHostApi>(
    svc: &ServiceConfig,
    planned_generation: u64,
    placements: &[(&HostConfig, u32)],
    docker_hosts: &HashMap<String, D>,
) -> Result<Option<u16>> {
    let Some(ports) = &svc.ports else {
        return Ok(None);
    };
    let Some(base) = ports.host_base else {
        return Ok(None);
    };

    let unique_hosts = placements
        .iter()
        .map(|(host, _)| host.name.as_str())
        .collect::<HashSet<_>>();
    let mut used_ports = HashSet::new();
    for host_name in unique_hosts {
        let docker = docker_hosts
            .get(host_name)
            .context(format!("No Docker connection for host {}", host_name))?;
        let all_containers = docker.list_containers(HashMap::new(), false).await?;
        for container in all_containers {
            if let Some(container_ports) = container.ports {
                for port in container_ports {
                    if let Some(public_port) = port.public_port {
                        used_ports.insert(public_port);
                    }
                }
            }
        }
    }

    let offset = crate::orchestrator::deploy::find_free_port_offset(
        base,
        svc.replicas,
        planned_generation,
        &used_ports,
    )?;
    Ok(Some(offset))
}

fn normalize_desired_instance(
    svc: &ServiceConfig,
    host: &HostConfig,
    instance: u32,
    config: ContainerCreateBody,
) -> NormalizedInstanceSpec {
    let host_config = config.host_config.unwrap_or_default();
    let env = normalize_env(config.env.unwrap_or_default());
    let mut labels = normalize_labels(config.labels.unwrap_or_default());
    labels.remove("korgi.generation");

    let ports = normalize_desired_ports(svc, host, instance, &host_config);

    NormalizedInstanceSpec {
        instance,
        host: host.name.clone(),
        image: config.image,
        env,
        labels,
        volumes: normalize_vec(host_config.binds.clone().unwrap_or_default()),
        restart_policy: normalize_restart_policy(host_config.restart_policy.clone()),
        healthcheck: normalize_healthcheck(config.healthcheck),
        resources: NormalizedResources {
            memory: host_config.memory,
            nano_cpus: host_config.nano_cpus,
        },
        cmd: config.cmd.unwrap_or_default(),
        entrypoint: config.entrypoint.unwrap_or_default(),
        ports,
    }
}

fn normalize_live_instances<D: DockerHostApi>(
    svc: &ServiceConfig,
    docker_hosts: &HashMap<String, D>,
    current_running: &[&KorgiContainer],
) -> Result<Vec<NormalizedInstanceSpec>> {
    let mut instances = Vec::new();
    for container in current_running {
        let docker = docker_hosts.get(&container.host_name).context(format!(
            "No Docker connection for host {}",
            container.host_name
        ))?;
        let inspect = futures::executor::block_on(docker.inspect_container(&container.id))
            .with_context(|| format!("Failed to inspect container {}", container.name))?;
        instances.push(normalize_live_instance(svc, container, inspect));
    }
    instances.sort_by_key(|instance| instance.instance);
    Ok(instances)
}

fn normalize_live_instance(
    svc: &ServiceConfig,
    container: &KorgiContainer,
    inspect: ContainerInspectResponse,
) -> NormalizedInstanceSpec {
    let host_config = inspect.host_config.unwrap_or_default();
    let config = inspect.config.unwrap_or_default();
    let mut labels = normalize_labels(config.labels.unwrap_or_default());
    labels.remove("korgi.generation");

    NormalizedInstanceSpec {
        instance: container.instance,
        host: container.host_name.clone(),
        image: config.image.or_else(|| Some(container.image.clone())),
        env: normalize_env(config.env.unwrap_or_default()),
        labels,
        volumes: normalize_vec(host_config.binds.clone().unwrap_or_default()),
        restart_policy: normalize_restart_policy(host_config.restart_policy.clone()),
        healthcheck: normalize_healthcheck(config.healthcheck),
        resources: NormalizedResources {
            memory: host_config.memory,
            nano_cpus: host_config.nano_cpus,
        },
        cmd: config.cmd.unwrap_or_default(),
        entrypoint: config.entrypoint.unwrap_or_default(),
        ports: normalize_live_ports(svc, container, &host_config),
    }
}

fn compute_field_changes(
    svc: &ServiceConfig,
    image: &str,
    current_generation: Option<u64>,
    planned_generation: u64,
    current: &[NormalizedInstanceSpec],
    desired: &[NormalizedInstanceSpec],
    current_placements: &[PlacementPlan],
    planned_placements: &[PlacementPlan],
) -> Vec<FieldChange> {
    let mut changes = Vec::new();

    let current_image = aggregate_value(current, |instance| json!(instance.image));
    let desired_image = json!(Some(image.to_string()));
    maybe_push_change(&mut changes, "image", current_image, desired_image);

    maybe_push_change(
        &mut changes,
        "generation",
        json!(current_generation),
        json!(planned_generation),
    );

    maybe_push_change(
        &mut changes,
        "placements",
        json!(current_placements),
        json!(planned_placements),
    );

    maybe_push_change(
        &mut changes,
        "instance_count",
        json!(current.len()),
        json!(desired.len()),
    );

    maybe_push_change(
        &mut changes,
        "ports",
        json!(
            current
                .iter()
                .map(|instance| &instance.ports)
                .collect::<Vec<_>>()
        ),
        json!(
            desired
                .iter()
                .map(|instance| &instance.ports)
                .collect::<Vec<_>>()
        ),
    );

    maybe_push_change(
        &mut changes,
        "env",
        aggregate_value(current, |instance| json!(instance.env)),
        aggregate_value(desired, |instance| json!(instance.env)),
    );

    maybe_push_change(
        &mut changes,
        "labels",
        aggregate_value(current, |instance| json!(instance.labels)),
        aggregate_value(desired, |instance| json!(instance.labels)),
    );

    maybe_push_change(
        &mut changes,
        "volumes",
        aggregate_value(current, |instance| json!(instance.volumes)),
        aggregate_value(desired, |instance| json!(instance.volumes)),
    );

    maybe_push_change(
        &mut changes,
        "restart_policy",
        aggregate_value(current, |instance| json!(instance.restart_policy)),
        aggregate_value(desired, |instance| json!(instance.restart_policy)),
    );

    maybe_push_change(
        &mut changes,
        "healthcheck",
        aggregate_value(current, |instance| json!(instance.healthcheck)),
        aggregate_value(desired, |instance| json!(instance.healthcheck)),
    );

    maybe_push_change(
        &mut changes,
        "resources",
        aggregate_value(current, |instance| json!(instance.resources)),
        aggregate_value(desired, |instance| json!(instance.resources)),
    );

    maybe_push_change(
        &mut changes,
        "command",
        aggregate_value(current, |instance| json!(instance.cmd)),
        aggregate_value(desired, |instance| json!(instance.cmd)),
    );

    maybe_push_change(
        &mut changes,
        "entrypoint",
        aggregate_value(current, |instance| json!(instance.entrypoint)),
        aggregate_value(desired, |instance| json!(instance.entrypoint)),
    );

    if changes.len() == 1 && changes[0].field == "generation" && current_generation.is_some() {
        changes.clear();
    }

    let _ = svc;
    changes
}

fn aggregate_value(
    instances: &[NormalizedInstanceSpec],
    value_fn: impl Fn(&NormalizedInstanceSpec) -> Value,
) -> Value {
    let values = instances.iter().map(value_fn).collect::<Vec<_>>();
    if values.is_empty() {
        Value::Null
    } else if values.windows(2).all(|window| window[0] == window[1]) {
        values[0].clone()
    } else {
        Value::Array(values)
    }
}

fn maybe_push_change(changes: &mut Vec<FieldChange>, field: &str, before: Value, after: Value) {
    if before != after {
        changes.push(FieldChange {
            field: field.to_string(),
            before,
            after,
        });
    }
}

fn normalize_env(values: Vec<String>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for value in values {
        if let Some((key, value)) = value.split_once('=') {
            env.insert(key.to_string(), value.to_string());
        }
    }
    env
}

fn normalize_labels(values: HashMap<String, String>) -> BTreeMap<String, String> {
    values.into_iter().collect()
}

fn normalize_vec(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

fn normalize_restart_policy(policy: Option<RestartPolicy>) -> Option<String> {
    policy.and_then(|policy| {
        policy.name.map(|name| {
            if let Some(max) = policy.maximum_retry_count {
                format!("{:?}:{}", name, max)
            } else {
                format!("{:?}", name)
            }
        })
    })
}

fn normalize_healthcheck(healthcheck: Option<HealthConfig>) -> Option<NormalizedHealthcheck> {
    healthcheck.map(|healthcheck| NormalizedHealthcheck {
        test: healthcheck.test.unwrap_or_default(),
        interval: healthcheck.interval,
        timeout: healthcheck.timeout,
        retries: healthcheck.retries,
        start_period: healthcheck.start_period,
    })
}

fn normalize_desired_ports(
    svc: &ServiceConfig,
    host: &HostConfig,
    instance: u32,
    host_config: &bollard::models::HostConfig,
) -> Vec<NormalizedPort> {
    let Some(ports) = &svc.ports else {
        return vec![];
    };

    let mut normalized = Vec::new();
    let mut binding_iter = host_config
        .port_bindings
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect::<Vec<_>>();
    binding_iter.sort_by(|a, b| a.0.cmp(&b.0));
    for (_key, values) in binding_iter {
        for value in values.unwrap_or_default() {
            normalized.push(NormalizedPort {
                instance,
                host: host.name.clone(),
                bind_ip: value
                    .host_ip
                    .unwrap_or_else(|| host.internal_addr().to_string()),
                container_port: ports.container,
                host_port: ports.host,
                host_base: ports.host_base.map(|base| base + instance as u16),
            });
        }
    }
    if normalized.is_empty() && (ports.host.is_some() || ports.host_base.is_some()) {
        normalized.push(NormalizedPort {
            instance,
            host: host.name.clone(),
            bind_ip: host.internal_addr().to_string(),
            container_port: ports.container,
            host_port: ports.host,
            host_base: ports.host_base.map(|base| base + instance as u16),
        });
    }
    normalized
}

fn normalize_live_ports(
    svc: &ServiceConfig,
    container: &KorgiContainer,
    host_config: &bollard::models::HostConfig,
) -> Vec<NormalizedPort> {
    let Some(ports) = &svc.ports else {
        return vec![];
    };

    let mut normalized = Vec::new();
    let mut binding_iter = host_config
        .port_bindings
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect::<Vec<_>>();
    binding_iter.sort_by(|a, b| a.0.cmp(&b.0));
    for (_key, values) in binding_iter {
        for value in values.unwrap_or_default() {
            normalized.push(NormalizedPort {
                instance: container.instance,
                host: container.host_name.clone(),
                bind_ip: value.host_ip.unwrap_or_else(|| "0.0.0.0".to_string()),
                container_port: ports.container,
                host_port: ports.host,
                host_base: ports.host_base.map(|base| base + container.instance as u16),
            });
        }
    }
    if normalized.is_empty() && (ports.host.is_some() || ports.host_base.is_some()) {
        normalized.push(NormalizedPort {
            instance: container.instance,
            host: container.host_name.clone(),
            bind_ip: "0.0.0.0".to_string(),
            container_port: ports.container,
            host_port: ports.host,
            host_base: ports.host_base.map(|base| base + container.instance as u16),
        });
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{
        Config, HostConfig, PortsConfig, ProjectConfig, ServiceConfig, TraefikConfig,
    };
    use crate::docker::mock::tests::{MockDockerHost, mock_container_summary};
    use bollard::models::{ContainerConfig, ContainerSummaryStateEnum};

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
                    h.internal_address = Some("10.0.0.1".to_string());
                    h.labels = vec!["web".to_string()];
                    h
                },
                {
                    let mut h = HostConfig::test_host("web2", "10.0.0.2");
                    h.internal_address = Some("10.0.0.2".to_string());
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
                svc.ports = Some(PortsConfig {
                    container: 8080,
                    host: None,
                    host_base: Some(9000),
                });
                svc.env.insert("FOO".to_string(), "bar".to_string());
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

    fn make_inspect(image: &str, env: &[&str]) -> ContainerInspectResponse {
        ContainerInspectResponse {
            config: Some(ContainerConfig {
                image: Some(image.to_string()),
                env: Some(env.iter().map(|v| v.to_string()).collect()),
                ..Default::default()
            }),
            host_config: Some(bollard::models::HostConfig::default()),
            ..Default::default()
        }
    }

    async fn setup_running_match(
        config: &Config,
        hosts: &HashMap<String, MockDockerHost>,
    ) -> Vec<NormalizedInstanceSpec> {
        hosts
            .get("web1")
            .unwrap()
            .add_container(mock_container_summary(
                "c1",
                "korgi-myapp-api-g1-0",
                "myapp",
                "api",
                1,
                0,
                "myapp/api:v1",
                ContainerSummaryStateEnum::RUNNING,
                "Up",
            ));
        hosts
            .get("web2")
            .unwrap()
            .add_container(mock_container_summary(
                "c2",
                "korgi-myapp-api-g1-1",
                "myapp",
                "api",
                1,
                1,
                "myapp/api:v1",
                ContainerSummaryStateEnum::RUNNING,
                "Up",
            ));

        let desired = normalize_desired_instances(
            config,
            &config.services[0],
            &config.services[0].image,
            2,
            &placement::place_replicas(&config.matching_hosts(&config.services[0]), 2),
            hosts,
        )
        .await
        .unwrap();
        hosts
            .get("web1")
            .unwrap()
            .set_inspect_response("c1", inspect_from_normalized(&desired[0]));
        hosts
            .get("web2")
            .unwrap()
            .set_inspect_response("c2", inspect_from_normalized(&desired[1]));
        desired
    }

    #[tokio::test]
    async fn test_diff_create_when_no_running_containers() {
        let config = test_config();
        let hosts = mock_hosts();

        let report = diff_services(&config, Some("api"), None, &hosts)
            .await
            .unwrap();
        assert_eq!(report.services[0].action, ServiceDiffAction::Create);
    }

    #[tokio::test]
    async fn test_diff_noop_when_running_matches() {
        let config = test_config();
        let hosts = mock_hosts();
        setup_running_match(&config, &hosts).await;

        let report = diff_services(&config, Some("api"), None, &hosts)
            .await
            .unwrap();
        assert_eq!(report.services[0].action, ServiceDiffAction::Noop);
    }

    #[tokio::test]
    async fn test_diff_replace_on_image_override() {
        let config = test_config();
        let hosts = mock_hosts();
        hosts
            .get("web1")
            .unwrap()
            .add_container(mock_container_summary(
                "c1",
                "korgi-myapp-api-g1-0",
                "myapp",
                "api",
                1,
                0,
                "myapp/api:v1",
                ContainerSummaryStateEnum::RUNNING,
                "Up",
            ));
        hosts
            .get("web1")
            .unwrap()
            .set_inspect_response("c1", make_inspect("myapp/api:v1", &[]));

        let report = diff_services(&config, Some("api"), Some("myapp/api:v2"), &hosts)
            .await
            .unwrap();
        assert_eq!(report.services[0].action, ServiceDiffAction::Replace);
        assert!(
            report.services[0]
                .field_changes
                .iter()
                .any(|change| change.field == "image")
        );
    }

    #[tokio::test]
    async fn test_diff_env_change_is_reported() {
        let config = test_config();
        let hosts = mock_hosts();
        let desired = setup_running_match(&config, &hosts).await;

        let mut changed = inspect_from_normalized(&desired[0]);
        changed.config.as_mut().unwrap().env = Some(vec!["FOO=baz".to_string()]);
        hosts
            .get("web1")
            .unwrap()
            .set_inspect_response("c1", changed);

        let report = diff_services(&config, Some("api"), None, &hosts)
            .await
            .unwrap();
        assert_eq!(report.services[0].action, ServiceDiffAction::Replace);
        assert!(
            report.services[0]
                .field_changes
                .iter()
                .any(|change| change.field == "env")
        );
    }

    #[tokio::test]
    async fn test_diff_placement_change_is_reported() {
        let mut config = test_config();
        let hosts = mock_hosts();
        setup_running_match(&config, &hosts).await;

        config.hosts[1].labels.clear();

        let report = diff_services(&config, Some("api"), None, &hosts)
            .await
            .unwrap();
        assert_eq!(report.services[0].action, ServiceDiffAction::Replace);
        assert!(
            report.services[0]
                .field_changes
                .iter()
                .any(|change| change.field == "placements")
        );
    }

    #[tokio::test]
    async fn test_diff_json_shape_is_stable() {
        let config = test_config();
        let hosts = mock_hosts();

        let report = diff_services(&config, Some("api"), None, &hosts)
            .await
            .unwrap();
        let json = serde_json::to_value(&report).unwrap();

        assert_eq!(json["project"], "myapp");
        assert_eq!(json["service_count"], 1);
        assert!(json["summary"].is_object());
        assert!(json["services"].is_array());
        assert_eq!(json["services"][0]["service"], "api");
        assert_eq!(json["services"][0]["action"], "create");
    }

    #[tokio::test]
    async fn test_stopped_generations_do_not_affect_baseline() {
        let config = test_config();
        let hosts = mock_hosts();
        hosts
            .get("web1")
            .unwrap()
            .add_container(mock_container_summary(
                "old",
                "korgi-myapp-api-g1-0",
                "myapp",
                "api",
                1,
                0,
                "myapp/api:v1",
                ContainerSummaryStateEnum::EXITED,
                "Exited",
            ));

        let report = diff_services(&config, Some("api"), None, &hosts)
            .await
            .unwrap();
        assert_eq!(report.services[0].action, ServiceDiffAction::Create);
    }

    fn inspect_from_normalized(instance: &NormalizedInstanceSpec) -> ContainerInspectResponse {
        let mut labels: HashMap<String, String> = instance.labels.clone().into_iter().collect();
        labels.insert("korgi.generation".to_string(), "1".to_string());
        labels.insert("korgi.instance".to_string(), instance.instance.to_string());
        let port_bindings = if instance.ports.is_empty() {
            None
        } else {
            let mut bindings = HashMap::new();
            for port in &instance.ports {
                let host_port = port
                    .host_base
                    .or(port.host_port)
                    .map(|value| value.to_string());
                bindings.insert(
                    format!("{}/tcp", port.container_port),
                    Some(vec![bollard::models::PortBinding {
                        host_ip: Some(port.bind_ip.clone()),
                        host_port,
                    }]),
                );
            }
            Some(bindings)
        };
        ContainerInspectResponse {
            config: Some(ContainerConfig {
                image: instance.image.clone(),
                env: Some(
                    instance
                        .env
                        .iter()
                        .map(|(key, value)| format!("{}={}", key, value))
                        .collect(),
                ),
                labels: Some(labels),
                cmd: Some(instance.cmd.clone()),
                entrypoint: Some(instance.entrypoint.clone()),
                healthcheck: instance.healthcheck.clone().map(|healthcheck| {
                    bollard::models::HealthConfig {
                        test: Some(healthcheck.test),
                        interval: healthcheck.interval,
                        timeout: healthcheck.timeout,
                        retries: healthcheck.retries,
                        start_period: healthcheck.start_period,
                        start_interval: None,
                    }
                }),
                ..Default::default()
            }),
            host_config: Some(bollard::models::HostConfig {
                binds: Some(instance.volumes.clone()),
                port_bindings,
                restart_policy: instance
                    .restart_policy
                    .as_ref()
                    .map(|policy| RestartPolicy {
                        name: Some(match policy.as_str() {
                            "ALWAYS" => bollard::models::RestartPolicyNameEnum::ALWAYS,
                            "NO" => bollard::models::RestartPolicyNameEnum::NO,
                            "ON_FAILURE" => bollard::models::RestartPolicyNameEnum::ON_FAILURE,
                            _ => bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED,
                        }),
                        maximum_retry_count: None,
                    }),
                memory: instance.resources.memory,
                nano_cpus: instance.resources.nano_cpus,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}
