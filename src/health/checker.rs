use anyhow::Result;
use std::time::{Duration, Instant};
use tracing::debug;

use crate::docker::traits::DockerHostApi;

/// Wait for a container to become healthy by polling docker inspect.
pub async fn wait_healthy<D: DockerHostApi>(
    docker: &D,
    container_id: &str,
    timeout: Duration,
) -> Result<()> {
    let start = Instant::now();
    let poll_interval = Duration::from_secs(2);

    loop {
        if start.elapsed() > timeout {
            anyhow::bail!(
                "Container {} did not become healthy within {}s on {}",
                container_id,
                timeout.as_secs(),
                docker.host_name()
            );
        }

        let inspect = docker.inspect_container(container_id).await?;

        if let Some(state) = &inspect.state {
            // Container died
            if state.running == Some(false) {
                anyhow::bail!(
                    "Container {} exited on {}",
                    container_id,
                    docker.host_name()
                );
            }

            // Check health status
            if let Some(health) = &state.health {
                let status_str = health
                    .status
                    .as_ref()
                    .map(|s| format!("{:?}", s).to_lowercase())
                    .unwrap_or_default();

                if status_str.contains("healthy") && !status_str.contains("unhealthy") {
                    debug!(
                        "Container {} healthy after {:.1}s",
                        container_id,
                        start.elapsed().as_secs_f64()
                    );
                    return Ok(());
                } else if status_str.contains("unhealthy") {
                    let log_msg = health
                        .log
                        .as_ref()
                        .and_then(|logs| logs.last())
                        .and_then(|l| l.output.clone())
                        .unwrap_or_default();
                    anyhow::bail!(
                        "Container {} unhealthy on {}: {}",
                        container_id,
                        docker.host_name(),
                        log_msg
                    );
                } else {
                    debug!("Container {} health status: {}", container_id, status_str);
                }
            } else {
                // No health check configured -- treat as healthy if running
                debug!(
                    "Container {} has no healthcheck, treating as healthy",
                    container_id
                );
                return Ok(());
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::mock::tests::MockDockerHost;
    use bollard::models::HealthStatusEnum;

    #[tokio::test]
    async fn test_wait_healthy_already_healthy() {
        let mock = MockDockerHost::new("web1");
        mock.set_health_status(Some(HealthStatusEnum::HEALTHY));

        let result = wait_healthy(&mock, "container-1", Duration::from_secs(5)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_wait_healthy_unhealthy_fails() {
        let mock = MockDockerHost::new("web1");
        mock.set_health_status(Some(HealthStatusEnum::UNHEALTHY));

        let result = wait_healthy(&mock, "container-1", Duration::from_secs(5)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unhealthy"));
    }

    #[tokio::test]
    async fn test_wait_healthy_container_exited_fails() {
        let mock = MockDockerHost::new("web1");
        mock.set_container_running(false);
        mock.set_health_status(Some(HealthStatusEnum::HEALTHY));

        let result = wait_healthy(&mock, "container-1", Duration::from_secs(5)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exited"));
    }

    #[tokio::test]
    async fn test_wait_healthy_no_healthcheck_treats_as_healthy() {
        let mock = MockDockerHost::new("web1");
        mock.set_health_status(None); // No health check configured

        let result = wait_healthy(&mock, "container-1", Duration::from_secs(5)).await;
        assert!(result.is_ok(), "Container without healthcheck should be treated as healthy if running");
    }
}
