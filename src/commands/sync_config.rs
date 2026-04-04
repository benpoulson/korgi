//! Syncs Traefik dynamic configuration after deploy/scale/rollback.
//!
//! Generates the file-provider YAML from current live state and writes it
//! into the running Traefik container(s) via docker exec.

use anyhow::{Context, Result};
use std::collections::HashMap;
use tracing::debug;

use crate::cli::output;
use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::state::LiveState;
use crate::orchestrator::traefik_config;

const TRAEFIK_CONTAINER_NAME: &str = "korgi-traefik";
const CONFIG_DIR: &str = "/etc/korgi";
const CONFIG_FILE: &str = "/etc/korgi/dynamic.yml";

/// Regenerate and push Traefik dynamic config to all Traefik hosts.
/// Call this after any operation that changes the container topology.
pub async fn sync_traefik_config(
    config: &Config,
    docker_hosts: &HashMap<String, DockerHost>,
) -> Result<()> {
    let Some(traefik) = &config.traefik else {
        debug!("No traefik config, skipping config sync");
        return Ok(());
    };

    // Query current live state across all hosts
    let state = LiveState::query(docker_hosts, &config.project.name).await?;

    // Generate the dynamic YAML
    let yaml = traefik_config::generate_dynamic_config(config, &state);
    debug!("Generated Traefik config ({} bytes)", yaml.len());

    // Write to each Traefik host's container
    for host_name in &traefik.hosts {
        let Some(docker) = docker_hosts.get(host_name) else {
            debug!("No Docker connection for Traefik host {}, skipping", host_name);
            continue;
        };

        // Check if Traefik container is running
        match docker.inspect_container(TRAEFIK_CONTAINER_NAME).await {
            Ok(info) => {
                let running = info
                    .state
                    .as_ref()
                    .and_then(|s| s.running)
                    .unwrap_or(false);
                if !running {
                    output::warn(&format!(
                        "Traefik not running on {}, skipping config sync",
                        host_name
                    ));
                    continue;
                }
            }
            Err(_) => {
                output::warn(&format!(
                    "Traefik container not found on {}, skipping config sync (run 'korgi traefik deploy' first)",
                    host_name
                ));
                continue;
            }
        }

        // Write the config file into the Traefik container via exec
        // Use printf to avoid heredoc/escaping issues
        let escaped = yaml.replace('\\', "\\\\").replace('\'', "'\\''");
        docker
            .exec_in_container(
                TRAEFIK_CONTAINER_NAME,
                &["sh", "-c", &format!("mkdir -p {} && printf '%s' '{}' > {}", CONFIG_DIR, escaped, CONFIG_FILE)],
            )
            .await
            .with_context(|| format!("Failed to write Traefik config on {}", host_name))?;

        debug!("Traefik config synced on {}", host_name);
    }

    output::success("Traefik routing config synced");
    Ok(())
}
