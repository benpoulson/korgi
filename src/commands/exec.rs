use anyhow::{Context, Result};
use std::collections::HashMap;

use crate::cli::output;
use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::state::LiveState;

pub async fn run(
    config: &Config,
    service: &str,
    cmd: &[String],
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    let state = LiveState::query(docker_hosts, &config.project.name).await?;
    let running = state.running_service_containers(service);

    let container = running
        .first()
        .context(format!("No running containers for service '{}'", service))?;

    let docker = docker_hosts
        .get(&container.host_name)
        .context(format!("No connection to host {}", container.host_name))?;

    output::info(&format!(
        "Executing on {} ({})",
        container.name, container.host_name
    ));

    // Use bollard exec API
    let exec_config = bollard::exec::CreateExecOptions {
        cmd: Some(cmd.to_vec()),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        ..Default::default()
    };

    let exec = docker
        .client()
        .create_exec(&container.id, exec_config)
        .await
        .context("Failed to create exec")?;

    let output = docker
        .client()
        .start_exec(&exec.id, None)
        .await
        .context("Failed to start exec")?;

    if let bollard::exec::StartExecResults::Attached { mut output, .. } = output {
        use futures::StreamExt;
        while let Some(Ok(msg)) = output.next().await {
            print!("{}", msg);
        }
    }

    Ok(())
}
