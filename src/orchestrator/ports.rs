//! Host port allocation for services with `ports.host_base`.
//!
//! The port a container is published on is chosen once, at creation, and recorded
//! in its `korgi.host_port` label. Everything that needs the port afterwards
//! (Traefik routing, HTTP health checks) reads it back from the container instead
//! of recomputing it from the config.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

use crate::docker::containers::host_port_from_bindings;
use crate::docker::labels;
use crate::docker::traits::DockerHostApi;

/// Ports that are taken on a host: published by any running container, plus the
/// recorded ports of every korgi container (stopped ones included), so stopped
/// rollback targets keep their port and can be restarted.
pub async fn collect_used_ports<D: DockerHostApi>(docker: &D) -> Result<HashSet<u16>> {
    let mut used = HashSet::new();
    let containers = docker
        .list_containers(HashMap::new(), true)
        .await
        .with_context(|| format!("Failed to list containers on {}", docker.host_name()))?;
    for c in &containers {
        if let Some(ports) = &c.ports {
            used.extend(ports.iter().filter_map(|p| p.public_port));
        }
        let Some(container_labels) = &c.labels else {
            continue;
        };
        match labels::parse_host_port(container_labels) {
            Some(Some(port)) => {
                used.insert(port);
            }
            Some(None) => {}
            None => {
                // Older korgi container without the label: its bindings tell us the
                // port even while it is stopped.
                if container_labels.contains_key("korgi.project")
                    && let Some(id) = &c.id
                {
                    let inspect = docker.inspect_container(id).await?;
                    if let Some(port) = host_port_from_bindings(inspect.host_config.as_ref()) {
                        used.insert(port);
                    }
                }
            }
        }
    }
    used.remove(&0);
    Ok(used)
}

/// Union of used ports across several hosts.
pub async fn collect_used_ports_on<D: DockerHostApi>(
    docker_hosts: &HashMap<String, D>,
    host_names: &[&str],
) -> Result<HashSet<u16>> {
    let mut used = HashSet::new();
    for host_name in host_names {
        let docker = docker_hosts
            .get(*host_name)
            .context(format!("No Docker connection for host {}", host_name))?;
        used.extend(collect_used_ports(docker).await?);
    }
    Ok(used)
}

/// Find the first block of `replicas` consecutive ports starting at
/// `base + offset` that are all free. Blocks are aligned to `replicas`; the search
/// starts at the generation's default block (`(generation - 1) * replicas`) and
/// wraps around, so ports freed by old generations are reused eventually.
pub fn find_free_port_offset(
    base: u16,
    replicas: u32,
    generation: u64,
    used_ports: &HashSet<u16>,
) -> Result<u16> {
    if replicas == 0 {
        return Ok(0);
    }
    let base32 = base as u32;
    let span = u16::MAX as u32 - base32 + 1;
    if replicas > span {
        anyhow::bail!(
            "{} replicas do not fit in the port range {}..=65535",
            replicas,
            base
        );
    }
    let slots = (span / replicas) as u64;
    let start_slot = generation.saturating_sub(1) % slots;
    for i in 0..slots {
        let slot = (start_slot + i) % slots;
        let offset = slot as u32 * replicas;
        let all_free = (0..replicas).all(|r| {
            let port = base32 + offset + r;
            !used_ports.contains(&(port as u16))
        });
        if all_free {
            return u16::try_from(offset).context("port offset overflow");
        }
    }
    anyhow::bail!(
        "Could not find {} free consecutive ports starting from {}",
        replicas,
        base
    )
}

/// Host port for instance `instance` of a block that starts at `base + offset`.
pub fn block_port(base: u16, offset: u16, instance: u32) -> Result<u16> {
    let port = base as u32 + offset as u32 + instance;
    u16::try_from(port)
        .ok()
        .filter(|p| *p != 0)
        .with_context(|| {
            format!(
                "host port {} + {} + {} is out of range (max 65535)",
                base, offset, instance
            )
        })
}

/// Pick `count` individual free ports at or above `base` (used when scaling up a
/// generation whose block may be partially taken).
pub fn find_free_ports(base: u16, count: usize, used_ports: &HashSet<u16>) -> Result<Vec<u16>> {
    let mut picked = Vec::with_capacity(count);
    let mut port = base as u32;
    while picked.len() < count {
        if port > u16::MAX as u32 {
            anyhow::bail!("Could not find {} free ports at or above {}", count, base);
        }
        let p = port as u16;
        if p != 0 && !used_ports.contains(&p) {
            picked.push(p);
        }
        port += 1;
    }
    Ok(picked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::mock::tests::*;
    use bollard::models::{ContainerSummaryStateEnum, PortSummary};

    #[test]
    fn test_port_offset_gen1_no_conflicts() {
        let used = HashSet::new();
        assert_eq!(find_free_port_offset(9001, 2, 1, &used).unwrap(), 0);
    }

    #[test]
    fn test_port_offset_gen2_no_conflicts() {
        let used: HashSet<u16> = [9001, 9002].into();
        assert_eq!(find_free_port_offset(9001, 2, 2, &used).unwrap(), 2);
    }

    #[test]
    fn test_port_offset_gen2_gen1_freed() {
        let used = HashSet::new();
        assert_eq!(find_free_port_offset(9001, 2, 2, &used).unwrap(), 2);
    }

    #[test]
    fn test_port_offset_skips_occupied_range() {
        let used: HashSet<u16> = [9001, 9002, 9003].into();
        assert_eq!(find_free_port_offset(9001, 2, 2, &used).unwrap(), 4);
    }

    #[test]
    fn test_port_offset_skips_multiple_occupied_ranges() {
        let used: HashSet<u16> = [9001, 9002, 9003, 9004, 9005, 9006].into();
        assert_eq!(find_free_port_offset(9001, 2, 1, &used).unwrap(), 6);
    }

    #[test]
    fn test_port_offset_three_replicas() {
        let used: HashSet<u16> = [9001, 9002, 9003, 9004, 9005, 9006].into();
        assert_eq!(find_free_port_offset(9001, 3, 3, &used).unwrap(), 6);
    }

    #[test]
    fn test_port_offset_gen3_after_gen1_freed_gen2_running() {
        let used: HashSet<u16> = [9003, 9004].into();
        assert_eq!(find_free_port_offset(9001, 2, 3, &used).unwrap(), 4);
    }

    #[test]
    fn test_port_offset_external_service_occupying_port() {
        let used: HashSet<u16> = [9001, 9002, 9005].into();
        assert_eq!(find_free_port_offset(9001, 2, 2, &used).unwrap(), 2);
    }

    #[test]
    fn test_port_offset_reuses_freed_ports() {
        let used: HashSet<u16> = [9007, 9008].into();
        assert_eq!(find_free_port_offset(9001, 2, 5, &used).unwrap(), 8);
    }

    #[test]
    fn test_port_offset_high_generation_wraps_without_overflow() {
        // Generation numbers far beyond the port space must not overflow u16.
        let used = HashSet::new();
        let offset = find_free_port_offset(60000, 4, 1_000_000, &used).unwrap();
        assert!(block_port(60000, offset, 3).is_ok());

        // Near the top of the range the search wraps back to the base.
        let used: HashSet<u16> = (65530..=65535).collect();
        let offset = find_free_port_offset(65530, 2, 2, &used);
        assert!(offset.is_err(), "every port in range is taken");
    }

    #[test]
    fn test_port_offset_replicas_exceed_range() {
        assert!(find_free_port_offset(65535, 2, 1, &HashSet::new()).is_err());
    }

    #[test]
    fn test_block_port_checked() {
        assert_eq!(block_port(9001, 4, 1).unwrap(), 9006);
        assert!(block_port(65535, 0, 1).is_err());
        assert!(block_port(65000, 600, 0).is_err());
    }

    #[test]
    fn test_find_free_ports() {
        let used: HashSet<u16> = [9001, 9003].into();
        assert_eq!(
            find_free_ports(9001, 3, &used).unwrap(),
            vec![9002, 9004, 9005]
        );
        assert!(find_free_ports(65535, 2, &HashSet::new()).is_err());
    }

    #[tokio::test]
    async fn test_collect_used_ports_reserves_stopped_korgi_containers() {
        let host = MockDockerHost::new("web1");
        // Stopped korgi container with recorded port (a rollback target).
        host.add_container(with_label(
            mock_container_summary(
                "stopped",
                "korgi-myapp-api-g1-0",
                "myapp",
                "api",
                1,
                0,
                "img",
                ContainerSummaryStateEnum::EXITED,
                "Exited",
            ),
            "korgi.host_port",
            "9001",
        ));
        // Running non-korgi container publishing a port.
        host.add_container(bollard::models::ContainerSummary {
            id: Some("other".into()),
            state: Some(ContainerSummaryStateEnum::RUNNING),
            ports: Some(vec![PortSummary {
                private_port: 80,
                public_port: Some(9003),
                ..Default::default()
            }]),
            ..Default::default()
        });
        // Stopped legacy korgi container (no label): port from inspected bindings.
        host.add_container(mock_container_summary(
            "legacy",
            "korgi-myapp-api-g0-0",
            "myapp",
            "api",
            0,
            0,
            "img",
            ContainerSummaryStateEnum::EXITED,
            "Exited",
        ));
        let mut bindings = HashMap::new();
        bindings.insert(
            "8080/tcp".to_string(),
            Some(vec![bollard::models::PortBinding {
                host_ip: None,
                host_port: Some("9002".into()),
            }]),
        );
        host.set_inspect_response(
            "legacy",
            bollard::models::ContainerInspectResponse {
                host_config: Some(bollard::models::HostConfig {
                    port_bindings: Some(bindings),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );

        let used = collect_used_ports(&host).await.unwrap();
        assert_eq!(used, [9001, 9002, 9003].into());
        assert!(
            host.get_calls()
                .contains(&DockerCall::ListContainers { all: true }),
            "must list stopped containers too"
        );
    }
}
