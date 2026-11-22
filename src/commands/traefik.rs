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

/// A parsed Traefik entrypoint address such as `:80`, `0.0.0.0:443`,
/// `[::]:8080` or `:443/udp`.
#[derive(Debug, PartialEq, Eq)]
pub struct EntrypointAddress {
    pub host_ip: Option<String>,
    pub port: u16,
    pub protocol: String,
}

/// Parse a Traefik entrypoint address (`[host]:port[/tcp|/udp]`).
pub fn parse_entrypoint_address(addr: &str) -> Result<EntrypointAddress> {
    let addr = addr.trim();
    let (hostport, protocol) = match addr.rsplit_once('/') {
        Some((hp, proto)) => {
            let proto = proto.to_ascii_lowercase();
            if proto != "tcp" && proto != "udp" {
                anyhow::bail!("unsupported protocol '{}' in '{}'", proto, addr);
            }
            (hp, proto)
        }
        None => (addr, "tcp".to_string()),
    };
    let (host, port_str) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => ("", hostport),
    };
    let port: u16 = port_str
        .parse()
        .ok()
        .filter(|p| *p != 0)
        .with_context(|| format!("invalid port '{}' in '{}'", port_str, addr))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host_ip = if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    };
    Ok(EntrypointAddress {
        host_ip,
        port,
        protocol,
    })
}

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
        let auth =
            crate::docker::registry::credentials_for_image(&traefik.image, &config.registries);
        docker.pull_image(&traefik.image, auth).await?;

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
            let ep = parse_entrypoint_address(addr)
                .with_context(|| format!("Invalid address for Traefik entrypoint '{}'", name))?;
            // Inside the container Traefik listens on all interfaces; a host IP in
            // the configured address only restricts the published port binding.
            let suffix = if ep.protocol == "udp" { "/udp" } else { "" };
            cmd.push(format!(
                "--entrypoints.{}.address=:{}{}",
                name, ep.port, suffix
            ));
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
        for (name, addr) in &traefik.entrypoints {
            let ep = parse_entrypoint_address(addr)
                .with_context(|| format!("Invalid address for Traefik entrypoint '{}'", name))?;
            port_bindings.insert(
                format!("{}/{}", ep.port, ep.protocol),
                Some(vec![PortBinding {
                    host_ip: Some(ep.host_ip.clone().unwrap_or_else(|| "0.0.0.0".to_string())),
                    host_port: Some(ep.port.to_string()),
                }]),
            );
        }

        let host_config = HostConfig {
            binds: Some(vec![
                "korgi-letsencrypt:/letsencrypt".to_string(),
                "korgi-config:/etc/korgi".to_string(),
            ]),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_entrypoint_address_forms() {
        let ep = parse_entrypoint_address(":80").unwrap();
        assert_eq!(
            (ep.host_ip, ep.port, ep.protocol.as_str()),
            (None, 80, "tcp")
        );

        let ep = parse_entrypoint_address("0.0.0.0:80").unwrap();
        assert_eq!(ep.host_ip.as_deref(), Some("0.0.0.0"));
        assert_eq!(ep.port, 80);

        let ep = parse_entrypoint_address("10.0.0.1:443").unwrap();
        assert_eq!(ep.host_ip.as_deref(), Some("10.0.0.1"));
        assert_eq!(ep.port, 443);

        let ep = parse_entrypoint_address("[::]:8080").unwrap();
        assert_eq!(ep.host_ip.as_deref(), Some("::"));
        assert_eq!(ep.port, 8080);

        let ep = parse_entrypoint_address(":443/udp").unwrap();
        assert_eq!((ep.port, ep.protocol.as_str()), (443, "udp"));

        let ep = parse_entrypoint_address("8000").unwrap();
        assert_eq!((ep.host_ip, ep.port), (None, 8000));
    }

    #[test]
    fn test_parse_entrypoint_address_rejects_garbage() {
        for bad in ["", ":", ":http", "0.0.0.0:99999", ":0", ":80/sctp"] {
            assert!(parse_entrypoint_address(bad).is_err(), "{}", bad);
        }
    }
}
