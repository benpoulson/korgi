use anyhow::Result;
use bollard::models::{ContainerInspectResponse, HealthStatusEnum};
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::debug;

use crate::config::duration::parse_duration;
use crate::config::types::{Config, HealthMode, ServiceConfig};
use crate::docker::traits::DockerHostApi;

/// Extra time allowed on top of the computed health deadline (SSH round trips,
/// image start-up jitter).
const DEADLINE_SLACK: Duration = Duration::from_secs(15);

/// How a single container is judged healthy.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckKind {
    /// Poll Docker's health status (injected HEALTHCHECK or the image's own).
    /// Falls back to [`CheckKind::Running`] when the container has no healthcheck.
    Docker,
    /// korgi polls this URL from outside via the container's host port.
    Http { url: String },
    /// No health endpoint: the container must stay running, not restarting, with a
    /// stable restart count for [`HealthSettings::stable_for`].
    Running,
}

/// Timing for a health-check run.
#[derive(Debug, Clone)]
pub struct HealthSettings {
    /// Delay between polls.
    pub interval: Duration,
    /// Timeout of each HTTP request (http mode).
    pub request_timeout: Duration,
    /// How long a container without a healthcheck must stay up and stable.
    pub stable_for: Duration,
    /// Overall deadline for all containers together.
    pub deadline: Duration,
}

impl HealthSettings {
    /// Derive settings from a service's `[health]` / `[deploy]` config.
    ///
    /// With a health section the deadline is
    /// `start_period + (interval + timeout) * (retries + 1) + slack`: enough for
    /// Docker to report a verdict either way. Without one it is
    /// `start_delay + slack`.
    pub fn for_service(svc: &ServiceConfig) -> Result<Self> {
        let deploy = Config::deploy_config(svc);
        let stable_for = Duration::from_secs(deploy.start_delay);
        match &svc.health {
            Some(h) => {
                let interval = parse_duration(&h.interval)?;
                let timeout = parse_duration(&h.timeout)?;
                let start_period = h
                    .start_period
                    .as_deref()
                    .map(parse_duration)
                    .transpose()?
                    .unwrap_or_default();
                let per_attempt = interval + timeout;
                let deadline = start_period
                    + per_attempt.saturating_mul(h.retries.saturating_add(1))
                    + stable_for
                    + DEADLINE_SLACK;
                Ok(Self {
                    interval: interval.clamp(Duration::from_millis(100), Duration::from_secs(5)),
                    request_timeout: timeout,
                    stable_for,
                    deadline,
                })
            }
            None => Ok(Self {
                interval: Duration::from_secs(1),
                request_timeout: Duration::from_secs(5),
                stable_for,
                deadline: stable_for + DEADLINE_SLACK,
            }),
        }
    }
}

/// One container to wait for.
pub struct HealthTarget<'a, D: DockerHostApi> {
    pub docker: &'a D,
    pub container_id: String,
    pub container_name: String,
    pub kind: CheckKind,
}

/// Which check a service's containers get.
pub fn check_kind_for(svc: &ServiceConfig, http_url: Option<String>) -> CheckKind {
    match svc.health.as_ref().map(|h| &h.mode) {
        None => CheckKind::Running,
        Some(HealthMode::Docker) | Some(HealthMode::Image) => CheckKind::Docker,
        Some(HealthMode::Http) => match http_url {
            Some(url) => CheckKind::Http { url },
            None => CheckKind::Running,
        },
    }
}

/// Wait until every target is healthy, checking all of them concurrently under a
/// single deadline. Returns the first failure (or a timeout listing the containers
/// that were still not healthy).
pub async fn wait_all_healthy<D: DockerHostApi>(
    targets: &[HealthTarget<'_, D>],
    settings: &HealthSettings,
) -> Result<()> {
    if targets.is_empty() {
        return Ok(());
    }
    let pending: Mutex<BTreeSet<String>> = Mutex::new(
        targets
            .iter()
            .map(|t| format!("{} on {}", t.container_name, t.docker.host_name()))
            .collect(),
    );
    let checks = targets.iter().map(|t| {
        let pending = &pending;
        async move {
            wait_one(t, settings).await?;
            pending.lock().unwrap().remove(&format!(
                "{} on {}",
                t.container_name,
                t.docker.host_name()
            ));
            Ok::<(), anyhow::Error>(())
        }
    });

    match tokio::time::timeout(settings.deadline, futures::future::try_join_all(checks)).await {
        Ok(result) => result.map(|_| ()),
        Err(_) => {
            let still: Vec<String> = pending.lock().unwrap().iter().cloned().collect();
            anyhow::bail!(
                "health checks did not pass within {:.0}s; not healthy: {}",
                settings.deadline.as_secs_f64(),
                still.join(", ")
            )
        }
    }
}

/// Wait for a single container (no deadline of its own beyond `timeout`).
pub async fn wait_healthy<D: DockerHostApi>(
    docker: &D,
    container_id: &str,
    kind: CheckKind,
    settings: &HealthSettings,
) -> Result<()> {
    let target = HealthTarget {
        docker,
        container_id: container_id.to_string(),
        container_name: container_id.to_string(),
        kind,
    };
    wait_all_healthy(std::slice::from_ref(&target), settings).await
}

async fn wait_one<D: DockerHostApi>(
    t: &HealthTarget<'_, D>,
    settings: &HealthSettings,
) -> Result<()> {
    match &t.kind {
        CheckKind::Docker => wait_docker(t, settings).await,
        CheckKind::Http { url } => wait_http(t, url, settings).await,
        CheckKind::Running => wait_running_stable(t, settings).await,
    }
}

fn ensure_running<D: DockerHostApi>(
    t: &HealthTarget<'_, D>,
    inspect: &ContainerInspectResponse,
) -> Result<()> {
    let state = inspect.state.as_ref();
    if state.and_then(|s| s.running) != Some(true) {
        anyhow::bail!(
            "Container {} exited on {}",
            t.container_name,
            t.docker.host_name()
        );
    }
    Ok(())
}

/// Docker HEALTHCHECK mode -- polls container inspect status.
async fn wait_docker<D: DockerHostApi>(
    t: &HealthTarget<'_, D>,
    settings: &HealthSettings,
) -> Result<()> {
    let start = Instant::now();
    loop {
        let inspect = t.docker.inspect_container(&t.container_id).await?;
        ensure_running(t, &inspect)?;
        let health = inspect.state.as_ref().and_then(|s| s.health.as_ref());
        match health.and_then(|h| h.status) {
            Some(HealthStatusEnum::HEALTHY) => {
                debug!(
                    "Container {} healthy after {:.1}s",
                    t.container_name,
                    start.elapsed().as_secs_f64()
                );
                return Ok(());
            }
            Some(HealthStatusEnum::UNHEALTHY) => {
                let log_msg = health
                    .and_then(|h| h.log.as_ref())
                    .and_then(|logs| logs.last())
                    .and_then(|l| l.output.clone())
                    .unwrap_or_default();
                anyhow::bail!(
                    "Container {} unhealthy on {}: {}",
                    t.container_name,
                    t.docker.host_name(),
                    log_msg.trim()
                );
            }
            Some(HealthStatusEnum::STARTING) => {
                debug!("Container {} health: starting", t.container_name);
            }
            Some(HealthStatusEnum::NONE) | Some(HealthStatusEnum::EMPTY) | None => {
                debug!(
                    "Container {} has no healthcheck; requiring it to stay up instead",
                    t.container_name
                );
                return wait_running_stable(t, settings).await;
            }
        }
        tokio::time::sleep(settings.interval).await;
    }
}

/// HTTP health check -- korgi polls the endpoint directly.
async fn wait_http<D: DockerHostApi>(
    t: &HealthTarget<'_, D>,
    url: &str,
    settings: &HealthSettings,
) -> Result<()> {
    let start = Instant::now();
    let client = reqwest::Client::builder()
        .timeout(settings.request_timeout)
        .build()?;
    loop {
        let inspect = t.docker.inspect_container(&t.container_id).await?;
        ensure_running(t, &inspect)?;

        match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                debug!(
                    "Container {} healthy after {:.1}s (HTTP {})",
                    t.container_name,
                    start.elapsed().as_secs_f64(),
                    resp.status()
                );
                return Ok(());
            }
            Ok(resp) => debug!("Health check {}: HTTP {}", url, resp.status()),
            Err(e) => debug!("Health check {}: {}", url, e),
        }
        tokio::time::sleep(settings.interval).await;
    }
}

/// Container must be running, not restarting, and keep the same restart count for
/// `stable_for`. Catches crash loops that a single "is it running?" check misses.
async fn wait_running_stable<D: DockerHostApi>(
    t: &HealthTarget<'_, D>,
    settings: &HealthSettings,
) -> Result<()> {
    let start = Instant::now();
    let poll = settings.interval.min(Duration::from_secs(1));
    let mut baseline: Option<i64> = None;
    loop {
        let inspect = t.docker.inspect_container(&t.container_id).await?;
        ensure_running(t, &inspect)?;
        if inspect.state.as_ref().and_then(|s| s.restarting) == Some(true) {
            anyhow::bail!(
                "Container {} is restarting on {}",
                t.container_name,
                t.docker.host_name()
            );
        }
        let count = inspect.restart_count.unwrap_or(0);
        match baseline {
            None => baseline = Some(count),
            Some(b) if count != b => anyhow::bail!(
                "Container {} restarted on {} (restart count {} -> {}); it is crash-looping",
                t.container_name,
                t.docker.host_name(),
                b,
                count
            ),
            Some(_) => {
                if start.elapsed() >= settings.stable_for {
                    debug!(
                        "Container {} stayed up for {:.1}s",
                        t.container_name,
                        start.elapsed().as_secs_f64()
                    );
                    return Ok(());
                }
            }
        }
        tokio::time::sleep(
            poll.min(settings.stable_for.saturating_sub(start.elapsed()))
                .max(Duration::from_millis(10)),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{DeployConfig, HealthConfig};
    use crate::docker::mock::tests::MockDockerHost;
    use bollard::models::HealthStatusEnum;

    fn fast(deadline_ms: u64) -> HealthSettings {
        HealthSettings {
            interval: Duration::from_millis(10),
            request_timeout: Duration::from_millis(100),
            stable_for: Duration::from_millis(50),
            deadline: Duration::from_millis(deadline_ms),
        }
    }

    #[tokio::test]
    async fn test_wait_healthy_already_healthy() {
        let mock = MockDockerHost::new("web1");
        mock.set_health_status(Some(HealthStatusEnum::HEALTHY));
        let result = wait_healthy(&mock, "container-1", CheckKind::Docker, &fast(1000)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_wait_healthy_unhealthy_fails() {
        let mock = MockDockerHost::new("web1");
        mock.set_health_status(Some(HealthStatusEnum::UNHEALTHY));
        let result = wait_healthy(&mock, "container-1", CheckKind::Docker, &fast(1000)).await;
        assert!(result.unwrap_err().to_string().contains("unhealthy"));
    }

    #[tokio::test]
    async fn test_wait_healthy_container_exited_fails() {
        let mock = MockDockerHost::new("web1");
        mock.set_container_running(false);
        mock.set_health_status(Some(HealthStatusEnum::HEALTHY));
        let result = wait_healthy(&mock, "container-1", CheckKind::Docker, &fast(1000)).await;
        assert!(result.unwrap_err().to_string().contains("exited"));
    }

    #[tokio::test]
    async fn test_no_healthcheck_falls_back_to_running_check() {
        for status in [
            None,
            Some(HealthStatusEnum::NONE),
            Some(HealthStatusEnum::EMPTY),
        ] {
            let mock = MockDockerHost::new("web1");
            mock.set_health_status(status);
            let result = wait_healthy(&mock, "c", CheckKind::Docker, &fast(1000)).await;
            assert!(result.is_ok(), "status {:?} should fall back", status);
            let inspects = mock
                .get_calls()
                .iter()
                .filter(|c| {
                    matches!(
                        c,
                        crate::docker::mock::tests::DockerCall::InspectContainer { .. }
                    )
                })
                .count();
            assert!(
                inspects >= 2,
                "fallback must observe the container over time"
            );
        }
    }

    #[tokio::test]
    async fn test_no_healthcheck_crash_loop_detected() {
        let mock = MockDockerHost::new("web1");
        mock.set_health_status(Some(HealthStatusEnum::NONE));
        mock.set_restart_counts(&[0, 0, 1]);
        let result = wait_healthy(&mock, "c", CheckKind::Docker, &fast(1000)).await;
        assert!(result.unwrap_err().to_string().contains("crash-looping"));
    }

    #[tokio::test]
    async fn test_running_check_requires_stable_restart_count() {
        let mock = MockDockerHost::new("web1");
        mock.set_restart_counts(&[2, 3]);
        let result = wait_healthy(&mock, "c", CheckKind::Running, &fast(1000)).await;
        assert!(result.is_err());

        let mock = MockDockerHost::new("web1");
        mock.set_restart_counts(&[2]);
        assert!(
            wait_healthy(&mock, "c", CheckKind::Running, &fast(1000))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_starting_forever_times_out_with_overall_deadline() {
        let mock = MockDockerHost::new("web1");
        mock.set_health_status(Some(HealthStatusEnum::STARTING));
        let started = Instant::now();
        let err = wait_healthy(&mock, "c1", CheckKind::Docker, &fast(150))
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(err.to_string().contains("did not pass"), "{}", err);
        assert!(err.to_string().contains("c1"));
    }

    #[tokio::test]
    async fn test_all_targets_checked_concurrently_under_one_deadline() {
        let a = MockDockerHost::new("web1");
        let b = MockDockerHost::new("web2");
        let settings = HealthSettings {
            stable_for: Duration::from_millis(300),
            ..fast(2000)
        };
        let targets = vec![
            HealthTarget {
                docker: &a,
                container_id: "a".into(),
                container_name: "a".into(),
                kind: CheckKind::Running,
            },
            HealthTarget {
                docker: &b,
                container_id: "b".into(),
                container_name: "b".into(),
                kind: CheckKind::Running,
            },
        ];
        let started = Instant::now();
        wait_all_healthy(&targets, &settings).await.unwrap();
        // Sequential would take >= 600ms.
        assert!(
            started.elapsed() < Duration::from_millis(550),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn test_http_check_uses_configured_request_timeout_and_deadline() {
        // Unroutable address: requests fail, the overall deadline ends the wait.
        let mock = MockDockerHost::new("web1");
        let started = Instant::now();
        let result = wait_healthy(
            &mock,
            "c",
            CheckKind::Http {
                url: "http://127.0.0.1:1/health".into(),
            },
            &fast(300),
        )
        .await;
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn test_deadline_independent_of_drain_seconds() {
        let mut svc = ServiceConfig::test_service("api", "api:v1");
        svc.deploy = Some(DeployConfig {
            drain_seconds: 0,
            start_delay: 0,
            rollback_keep: 2,
        });
        svc.health = Some(HealthConfig {
            mode: HealthMode::Docker,
            path: "/health".into(),
            interval: "5s".into(),
            timeout: "3s".into(),
            retries: 3,
            start_period: Some("10s".into()),
        });
        let s = HealthSettings::for_service(&svc).unwrap();
        // 10 + (5+3)*4 + 15 slack
        assert_eq!(s.deadline, Duration::from_secs(10 + 32 + 15));
        assert_eq!(s.request_timeout, Duration::from_secs(3));
        assert_eq!(s.interval, Duration::from_secs(5));

        svc.health = None;
        svc.deploy.as_mut().unwrap().start_delay = 7;
        let s = HealthSettings::for_service(&svc).unwrap();
        assert_eq!(s.stable_for, Duration::from_secs(7));
        assert!(s.deadline > s.stable_for);
    }

    #[test]
    fn test_check_kind_for_modes() {
        let mut svc = ServiceConfig::test_service("api", "api:v1");
        assert_eq!(check_kind_for(&svc, None), CheckKind::Running);
        svc.health = Some(HealthConfig {
            mode: HealthMode::Image,
            path: String::new(),
            interval: "5s".into(),
            timeout: "3s".into(),
            retries: 3,
            start_period: None,
        });
        assert_eq!(check_kind_for(&svc, None), CheckKind::Docker);
        svc.health.as_mut().unwrap().mode = HealthMode::Http;
        assert_eq!(
            check_kind_for(&svc, Some("http://x:1/h".into())),
            CheckKind::Http {
                url: "http://x:1/h".into()
            }
        );
    }
}
