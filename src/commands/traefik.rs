use anyhow::{Context, Result};
use bollard::models::{
    ContainerCreateBody, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use futures::StreamExt;
use std::collections::HashMap;

use crate::cli::output;
use crate::commands::sync_config;
use crate::config::types::Config;
use crate::docker::host::DockerHost;

const TRAEFIK_CONTAINER_NAME: &str = "korgi-traefik";

pub async fn deploy(config: &Config, docker_hosts: &HashMap<String, DockerHost>) -> Result<()> {
    let traefik = config
        .traefik
        .as_ref()
        .context("No [traefik] section in config")?;

    let traefik_hosts = config.traefik_host_names();
    output::info(&format!(
        "Deploying Traefik ({}) to {} hosts",
        traefik.image,
        traefik_hosts.len()
    ));

    for host_name in &traefik_hosts {
        let docker = docker_hosts
            .get(host_name)
            .context(format!("No Docker connection for host {}", host_name))?;

        let pb = output::spinner(&format!("Setting up Traefik on {}...", host_name));

        // Ensure network exists
        docker.ensure_network(&traefik.network).await?;

        // Pull image
        docker.pull_image(&traefik.image, None).await?;

        // Remove existing traefik container if any
        let _ = docker.stop_container(TRAEFIK_CONTAINER_NAME, 10).await;
        let _ = docker.remove_container(TRAEFIK_CONTAINER_NAME, true).await;

        // Build Traefik command args
        // Korgi manages all routing via the file provider -- no Docker provider needed
        let mut cmd = vec![
            "--providers.file.directory=/etc/korgi/".to_string(),
            "--providers.file.watch=true".to_string(),
        ];

        for (name, addr) in &traefik.entrypoints {
            cmd.push(format!("--entrypoints.{}.address={}", name, addr));
        }

        if let Some(acme) = &traefik.acme {
            cmd.push("--certificatesresolvers.letsencrypt.acme.tlschallenge=true".to_string());
            cmd.push(format!(
                "--certificatesresolvers.letsencrypt.acme.email={}",
                acme.email
            ));
            cmd.push(format!(
                "--certificatesresolvers.letsencrypt.acme.storage={}",
                acme.storage
            ));

            // Auto-redirect HTTP to HTTPS when ACME is configured
            if traefik.entrypoints.contains_key("web")
                && traefik.entrypoints.contains_key("websecure")
            {
                cmd.push("--entrypoints.web.http.redirections.entrypoint.to=websecure".to_string());
                cmd.push("--entrypoints.web.http.redirections.entrypoint.scheme=https".to_string());
            }
        }

        // Port bindings
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        for addr in traefik.entrypoints.values() {
            let port = addr.trim_start_matches(':');
            port_bindings.insert(
                format!("{}/tcp", port),
                Some(vec![PortBinding {
                    host_ip: Some("0.0.0.0".to_string()),
                    host_port: Some(port.to_string()),
                }]),
            );
        }

        let host_config = HostConfig {
            binds: Some(vec!["korgi-letsencrypt:/letsencrypt".to_string()]),
            port_bindings: Some(port_bindings),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            network_mode: Some(traefik.network.clone()),
            ..Default::default()
        };

        let container_config = ContainerCreateBody {
            image: Some(traefik.image.clone()),
            cmd: Some(cmd),
            host_config: Some(host_config),
            labels: Some(HashMap::from([
                ("korgi.component".to_string(), "traefik".to_string()),
                ("korgi.project".to_string(), config.project.name.clone()),
            ])),
            ..Default::default()
        };

        let id = docker
            .create_container(TRAEFIK_CONTAINER_NAME, container_config)
            .await?;
        docker.start_container(&id).await?;

        pb.finish_and_clear();
        output::success(&format!("Traefik running on {}", host_name));
    }

    // Sync current routing config into Traefik
    sync_config::sync_traefik_config(config, docker_hosts).await?;

    Ok(())
}

pub async fn status(config: &Config, docker_hosts: &HashMap<String, DockerHost>) -> Result<()> {
    config
        .traefik
        .as_ref()
        .context("No [traefik] section in config")?;
    let traefik_hosts = config.traefik_host_names();
    for host_name in &traefik_hosts {
        let docker = docker_hosts
            .get(host_name)
            .context(format!("No Docker connection for host {}", host_name))?;

        match docker.inspect_container(TRAEFIK_CONTAINER_NAME).await {
            Ok(info) => {
                let state = info
                    .state
                    .as_ref()
                    .and_then(|s| s.status.as_ref())
                    .map(|s| format!("{:?}", s))
                    .unwrap_or("unknown".to_string());
                let image = info
                    .config
                    .as_ref()
                    .and_then(|c| c.image.clone())
                    .unwrap_or_default();
                output::success(&format!("{}: {} ({})", host_name, state, image));
            }
            Err(_) => {
                output::error(&format!("{}: Traefik not running", host_name));
            }
        }
    }

    Ok(())
}

pub async fn logs(
    config: &Config,
    docker_hosts: &HashMap<String, DockerHost>,
    follow: bool,
) -> Result<()> {
    config
        .traefik
        .as_ref()
        .context("No [traefik] section in config")?;
    let traefik_hosts = config.traefik_host_names();
    let host_name = traefik_hosts
        .first()
        .context("No traefik hosts configured")?;
    let docker = docker_hosts
        .get(host_name)
        .context(format!("No Docker connection for host {}", host_name))?;

    let mut stream = docker.logs(TRAEFIK_CONTAINER_NAME, follow, "100");
    while let Some(result) = stream.next().await {
        match result {
            Ok(log) => print!("{}", log),
            Err(e) => {
                output::error(&format!("Log stream error: {}", e));
                break;
            }
        }
    }

    Ok(())
}
