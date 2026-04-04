use anyhow::Result;
use std::collections::HashMap;

use crate::cli::output;
use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::state::LiveState;

pub async fn run(
    config: &Config,
    service_filter: Option<&str>,
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    let state = LiveState::query(docker_hosts, &config.project.name).await?;

    let containers: Vec<_> = if let Some(svc) = service_filter {
        state.service_containers(svc).into_iter().collect()
    } else {
        state.containers.iter().collect()
    };

    if containers.is_empty() {
        output::info("No containers to destroy");
        return Ok(());
    }

    let desc = service_filter
        .map(|s| format!("service '{}'", s))
        .unwrap_or("all services".to_string());
    output::info(&format!(
        "Destroying {} containers for {}",
        containers.len(),
        desc
    ));

    let pb = output::progress_bar(containers.len() as u64, "Destroying containers");
    for container in &containers {
        if let Some(docker) = docker_hosts.get(&container.host_name) {
            if container.state == "running" {
                docker.stop_container(&container.id, 10).await.ok();
            }
            docker.remove_container(&container.id, true).await.ok();
        }
        pb.inc(1);
    }
    pb.finish_and_clear();

    output::success(&format!("Destroyed {} containers", containers.len()));
    Ok(())
}
