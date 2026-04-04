use anyhow::{Context, Result};
use futures::StreamExt;
use std::collections::HashMap;

use crate::cli::output;
use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::state::LiveState;

pub async fn run(
    config: &Config,
    service: &str,
    follow: bool,
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    let state = LiveState::query(docker_hosts, &config.project.name).await?;
    let running = state.running_service_containers(service);

    if running.is_empty() {
        output::warn(&format!("No running containers for service '{}'", service));
        return Ok(());
    }

    // If following, only tail the first container; otherwise show all
    let containers = if follow {
        output::info(&format!(
            "Following logs from {} ({})",
            running[0].name, running[0].host_name
        ));
        vec![running[0]]
    } else {
        running
    };

    for container in containers {
        let docker = docker_hosts
            .get(&container.host_name)
            .context(format!("No connection to host {}", container.host_name))?;

        if !follow {
            output::header(&format!("{} ({})", container.name, container.host_name));
        }

        let mut stream = docker.logs(&container.id, follow, "100");
        while let Some(result) = stream.next().await {
            match result {
                Ok(log) => print!("{}", log),
                Err(e) => {
                    output::error(&format!("Log error: {}", e));
                    break;
                }
            }
        }
    }

    Ok(())
}
