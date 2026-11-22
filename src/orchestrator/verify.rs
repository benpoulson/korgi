//! Building blocks shared by deploy, rollback and scale: health-gating a set of
//! containers, tracking containers created by an operation so they can be removed
//! on failure, and stopping/removing containers concurrently with errors surfaced.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::cli::output;
use crate::config::types::{Config, HealthMode, ServiceConfig};
use crate::docker::containers::KorgiContainer;
use crate::docker::registry;
use crate::docker::traits::DockerHostApi;
use crate::health::{self, HealthSettings, HealthTarget};

/// A container an operation created or started and must verify.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerRef {
    pub service: String,
    pub host_name: String,
    pub id: String,
    pub name: String,
    pub host_port: Option<u16>,
}

impl From<&KorgiContainer> for ContainerRef {
    fn from(c: &KorgiContainer) -> Self {
        Self {
            service: c.service.clone(),
            host_name: c.host_name.clone(),
            id: c.id.clone(),
            name: c.name.clone(),
            host_port: c.host_port,
        }
    }
}

/// Containers created during an operation that have not yet been handed over to
/// Traefik. Shared with the caller so that, if the operation is cancelled
/// (Ctrl-C) mid-way, everything created so far can still be removed.
#[derive(Debug, Default)]
pub struct CreatedContainers {
    inner: Mutex<Vec<ContainerRef>>,
}

impl CreatedContainers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, c: ContainerRef) {
        self.inner.lock().unwrap().push(c);
    }

    pub fn snapshot(&self) -> Vec<ContainerRef> {
        self.inner.lock().unwrap().clone()
    }

    pub fn for_service(&self, service: &str) -> Vec<ContainerRef> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.service == service)
            .cloned()
            .collect()
    }

    /// Remove and return the entries of one service.
    pub fn take_service(&self, service: &str) -> Vec<ContainerRef> {
        let mut inner = self.inner.lock().unwrap();
        let (taken, kept): (Vec<_>, Vec<_>) = inner.drain(..).partition(|c| c.service == service);
        *inner = kept;
        taken
    }

    /// Remove and return every entry.
    pub fn take_all(&self) -> Vec<ContainerRef> {
        std::mem::take(&mut *self.inner.lock().unwrap())
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }
}

/// Health-check URL for an HTTP-mode container, built from the host's internal
/// address and the port the container actually published.
pub fn http_health_url(config: &Config, svc: &ServiceConfig, c: &ContainerRef) -> Option<String> {
    let health = svc.health.as_ref()?;
    if health.mode != HealthMode::Http {
        return None;
    }
    let host = config.hosts.iter().find(|h| h.name == c.host_name)?;
    let port = c.host_port?;
    Some(format!(
        "http://{}:{}{}",
        host.internal_addr(),
        port,
        health.path
    ))
}

/// Wait until all `containers` (of service `svc`) are healthy, concurrently and
/// under one deadline derived from the service's health config.
pub async fn verify_healthy<D: DockerHostApi>(
    config: &Config,
    svc: &ServiceConfig,
    containers: &[ContainerRef],
    docker_hosts: &HashMap<String, D>,
) -> Result<()> {
    let settings = HealthSettings::for_service(svc)?;
    verify_healthy_with(config, svc, containers, docker_hosts, &settings).await
}

pub async fn verify_healthy_with<D: DockerHostApi>(
    config: &Config,
    svc: &ServiceConfig,
    containers: &[ContainerRef],
    docker_hosts: &HashMap<String, D>,
    settings: &HealthSettings,
) -> Result<()> {
    let mut targets = Vec::with_capacity(containers.len());
    for c in containers {
        let docker = docker_hosts
            .get(&c.host_name)
            .context(format!("No Docker connection for host {}", c.host_name))?;
        let url = http_health_url(config, svc, c);
        if svc
            .health
            .as_ref()
            .is_some_and(|h| h.mode == HealthMode::Http)
            && url.is_none()
        {
            anyhow::bail!(
                "Cannot HTTP health-check {} on {}: it has no published host port",
                c.name,
                c.host_name
            );
        }
        targets.push(HealthTarget {
            docker,
            container_id: c.id.clone(),
            container_name: c.name.clone(),
            kind: health::check_kind_for(svc, url),
        });
    }
    health::wait_all_healthy(&targets, settings).await
}

/// Stop containers concurrently. Every stop is attempted; failures are collected
/// into one error.
pub async fn stop_all<D: DockerHostApi>(
    docker_hosts: &HashMap<String, D>,
    containers: &[ContainerRef],
    timeout_secs: i64,
) -> Result<()> {
    let stops = containers.iter().map(|c| async move {
        let docker = docker_hosts
            .get(&c.host_name)
            .with_context(|| format!("No Docker connection for host {}", c.host_name))?;
        docker
            .stop_container(&c.id, timeout_secs)
            .await
            .with_context(|| format!("Failed to stop {} on {}", c.name, c.host_name))
    });
    collect_errors(futures::future::join_all(stops).await)
}

/// Force-remove containers concurrently, collecting failures into one error.
pub async fn remove_all<D: DockerHostApi>(
    docker_hosts: &HashMap<String, D>,
    containers: &[ContainerRef],
) -> Result<()> {
    let removals = containers.iter().map(|c| async move {
        let docker = docker_hosts
            .get(&c.host_name)
            .with_context(|| format!("No Docker connection for host {}", c.host_name))?;
        docker
            .remove_container(&c.id, true)
            .await
            .with_context(|| format!("Failed to remove {} on {}", c.name, c.host_name))
    });
    collect_errors(futures::future::join_all(removals).await)
}

/// Remove containers created by a failed or cancelled operation, reporting (not
/// propagating) failures so the original error stays the one returned.
pub async fn discard_created<D: DockerHostApi>(
    docker_hosts: &HashMap<String, D>,
    containers: &[ContainerRef],
) {
    if containers.is_empty() {
        return;
    }
    output::info(&format!(
        "Removing {} new container(s); existing containers keep serving",
        containers.len()
    ));
    if let Err(e) = remove_all(docker_hosts, containers).await {
        output::error(&format!("Cleanup incomplete: {:#}", e));
    }
}

/// Fold per-item results into a single error listing every failure.
pub fn collect_errors(results: Vec<Result<()>>) -> Result<()> {
    let errors: Vec<String> = results
        .into_iter()
        .filter_map(|r| r.err().map(|e| format!("{:#}", e)))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", errors.join("; "))
    }
}

/// Make sure `image` is present on a host, pulling it with registry credentials
/// when missing.
pub async fn ensure_image<D: DockerHostApi>(
    docker: &D,
    config: &Config,
    image: &str,
) -> Result<()> {
    if docker.image_exists(image).await? {
        return Ok(());
    }
    output::info(&format!(
        "Image {} missing on {}, pulling...",
        image,
        docker.host_name()
    ));
    let auth = registry::credentials_for_image(image, &config.registries);
    docker
        .pull_image(image, auth)
        .await
        .with_context(|| format!("Failed to pull {} on {}", image, docker.host_name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(service: &str, id: &str) -> ContainerRef {
        ContainerRef {
            service: service.into(),
            host_name: "web1".into(),
            id: id.into(),
            name: id.into(),
            host_port: None,
        }
    }

    #[test]
    fn test_created_containers_take_service() {
        let created = CreatedContainers::new();
        created.push(r("a", "1"));
        created.push(r("b", "2"));
        created.push(r("a", "3"));
        let a = created.take_service("a");
        assert_eq!(a.len(), 2);
        assert_eq!(created.snapshot(), vec![r("b", "2")]);
        assert_eq!(created.take_all().len(), 1);
        assert!(created.is_empty());
    }

    #[test]
    fn test_collect_errors() {
        assert!(collect_errors(vec![Ok(()), Ok(())]).is_ok());
        let err = collect_errors(vec![
            Ok(()),
            Err(anyhow::anyhow!("x failed")),
            Err(anyhow::anyhow!("y failed")),
        ])
        .unwrap_err()
        .to_string();
        assert!(err.contains("x failed") && err.contains("y failed"));
    }
}
