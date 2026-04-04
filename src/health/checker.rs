use anyhow::Result;
use std::time::{Duration, Instant};
use tracing::debug;

use crate::docker::host::DockerHost;

/// Wait for a container to become healthy by polling docker inspect.
pub async fn wait_healthy(
    docker: &DockerHost,
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
                docker.host_name
            );
        }

        let inspect = docker.inspect_container(container_id).await?;

        if let Some(state) = &inspect.state {
            // Container died
            if state.running == Some(false) {
                anyhow::bail!(
                    "Container {} exited on {}",
                    container_id,
                    docker.host_name
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
                        docker.host_name,
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
