//! Pushes the generated Traefik dynamic config into every Traefik container.
//!
//! Traefik runs with only the file provider, watching `/etc/korgi/`. A sync is
//! successful only when every Traefik host has the new file in place, verified by
//! reading it back. Callers rely on that: old containers are never stopped unless
//! the sync returned `Ok`.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use tracing::debug;

use crate::cli::output;
use crate::config::types::Config;
use crate::docker::traits::DockerExecApi;
use crate::orchestrator::state::LiveState;
use crate::orchestrator::traefik_config;

pub const TRAEFIK_CONTAINER_NAME: &str = "korgi-traefik";
pub const CONFIG_DIR: &str = "/etc/korgi";
pub const CONFIG_FILE: &str = "/etc/korgi/dynamic.yml";
/// Temporary file in the same directory (same filesystem, so `mv` is an atomic
/// rename). Its extension is not one Traefik's file provider loads, so a
/// half-written temp file is never parsed.
pub const TEMP_FILE: &str = "/etc/korgi/.dynamic.yml.tmp";

/// Keep each `sh -c` argument well below Linux's 128 KiB per-argument limit.
const MAX_CHUNK_BYTES: usize = 64 * 1024;

/// Regenerate and push Traefik dynamic config to all Traefik hosts.
/// Call this after any operation that changes the container topology.
pub async fn sync_traefik_config<D: DockerExecApi>(
    config: &Config,
    docker_hosts: &HashMap<String, D>,
) -> Result<()> {
    sync_traefik_config_excluding(config, docker_hosts, &HashSet::new()).await
}

/// Like [`sync_traefik_config`], but leaves out the given container ids even if
/// they are still running (used before stopping them).
pub async fn sync_traefik_config_excluding<D: DockerExecApi>(
    config: &Config,
    docker_hosts: &HashMap<String, D>,
    exclude_ids: &HashSet<String>,
) -> Result<()> {
    if config.traefik.is_none() {
        debug!("No traefik config, skipping config sync");
        return Ok(());
    }

    let traefik_hosts = config.traefik_host_names();
    if traefik_hosts.is_empty() {
        debug!("No traefik hosts, skipping config sync");
        return Ok(());
    }

    // Every Traefik host must be reachable and running Traefik before anything is
    // written: a host that silently keeps an old config would keep routing to
    // containers that are about to be stopped.
    let mut targets = Vec::new();
    for host_name in &traefik_hosts {
        let docker = docker_hosts.get(host_name).with_context(|| {
            format!(
                "No Docker connection for Traefik host {}; cannot update routing",
                host_name
            )
        })?;
        let info = docker
            .inspect_container(TRAEFIK_CONTAINER_NAME)
            .await
            .with_context(|| {
                format!(
                    "Traefik container not found on {} (run 'korgi traefik deploy' first); \
                     refusing to continue without updating its routing",
                    host_name
                )
            })?;
        let running = info.state.as_ref().and_then(|s| s.running).unwrap_or(false);
        if !running {
            anyhow::bail!(
                "Traefik is not running on {}; refusing to continue without updating its routing",
                host_name
            );
        }
        targets.push((host_name.as_str(), docker));
    }

    let mut state = LiveState::query(docker_hosts, &config.project.name).await?;
    state.containers.retain(|c| !exclude_ids.contains(&c.id));

    let yaml = traefik_config::generate_dynamic_config(config, &state);
    debug!("Generated Traefik config ({} bytes)", yaml.len());

    for (host_name, docker) in targets {
        write_config(docker, &yaml)
            .await
            .with_context(|| format!("Failed to update Traefik config on {}", host_name))?;
        debug!("Traefik config synced on {}", host_name);
    }

    output::success("Traefik routing config synced");
    Ok(())
}

/// Quote a string for POSIX `sh` using single quotes. Inside single quotes nothing
/// is special except `'` itself, which is written as `'\''`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Shell scripts that write `content` to the temp file in chunks and atomically
/// rename it over the config file in the last one.
pub fn write_scripts(content: &str) -> Vec<String> {
    let chunks = split_chunks(content, MAX_CHUNK_BYTES);
    let last = chunks.len() - 1;
    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let redirect = if i == 0 { ">" } else { ">>" };
            let mut script = format!(
                "mkdir -p {} && printf '%s' {} {} {}",
                sh_quote(CONFIG_DIR),
                sh_quote(chunk),
                redirect,
                sh_quote(TEMP_FILE)
            );
            if i == last {
                script.push_str(&format!(
                    " && mv -f {} {}",
                    sh_quote(TEMP_FILE),
                    sh_quote(CONFIG_FILE)
                ));
            }
            script
        })
        .collect()
}

fn split_chunks(content: &str, max: usize) -> Vec<&str> {
    if content.is_empty() {
        return vec![""];
    }
    let mut chunks = Vec::new();
    let mut rest = content;
    while !rest.is_empty() {
        let mut end = rest.len().min(max);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (chunk, tail) = rest.split_at(end);
        chunks.push(chunk);
        rest = tail;
    }
    chunks
}

async fn write_config<D: DockerExecApi>(docker: &D, yaml: &str) -> Result<()> {
    for script in write_scripts(yaml) {
        docker
            .exec_in_container(TRAEFIK_CONTAINER_NAME, &["sh", "-c", &script])
            .await?;
    }
    // Confirm the file Traefik watches now holds exactly what we generated.
    let read_back = docker
        .exec_in_container(
            TRAEFIK_CONTAINER_NAME,
            &["sh", "-c", &format!("cat {}", sh_quote(CONFIG_FILE))],
        )
        .await
        .context("Failed to read back Traefik config")?;
    // Trailing whitespace is ignored in case the exec transport trims output.
    if read_back.trim_end() != yaml.trim_end() {
        anyhow::bail!(
            "Traefik config read back from {} does not match what was written ({} vs {} bytes)",
            CONFIG_FILE,
            read_back.len(),
            yaml.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::*;
    use crate::docker::mock::tests::*;
    use bollard::models::ContainerSummaryStateEnum;

    pub fn config_with_traefik() -> Config {
        Config {
            project: ProjectConfig {
                name: "myapp".to_string(),
                secrets: None,
            },
            registries: vec![],
            hosts: vec![
                {
                    let mut h = HostConfig::test_host("lb1", "10.0.0.100");
                    h.role = HostRole::Lb;
                    h
                },
                {
                    let mut h = HostConfig::test_host("lb2", "10.0.0.101");
                    h.role = HostRole::Lb;
                    h
                },
                {
                    let mut h = HostConfig::test_host("web1", "10.0.0.1");
                    h.labels = vec!["web".to_string()];
                    h
                },
            ],
            traefik: Some(TraefikConfig {
                image: "traefik:v3.2".to_string(),
                hosts: vec![],
                entrypoints: HashMap::new(),
                network: "korgi-traefik".to_string(),
                acme: None,
            }),
            services: vec![{
                let mut svc = ServiceConfig::test_service("api", "myapp/api:v1");
                svc.placement_labels = vec!["web".to_string()];
                svc.routing = Some(RoutingConfig {
                    rule: "Host(`it's.example.com`) && Path(`/a\\b`)".to_string(),
                    entrypoints: vec!["web".to_string()],
                    tls: false,
                    lb_strategy: None,
                    sticky: None,
                });
                svc.ports = Some(PortsConfig {
                    container: 8080,
                    host: None,
                    host_base: Some(9001),
                });
                svc
            }],
        }
    }

    fn hosts() -> HashMap<String, MockDockerHost> {
        let mut hosts = HashMap::new();
        for name in ["lb1", "lb2", "web1"] {
            hosts.insert(name.to_string(), MockDockerHost::new(name));
        }
        hosts.get("web1").unwrap().add_container(with_label(
            mock_container_summary(
                "c1",
                "korgi-myapp-api-g1-0",
                "myapp",
                "api",
                1,
                0,
                "myapp/api:v1",
                ContainerSummaryStateEnum::RUNNING,
                "Up",
            ),
            "korgi.host_port",
            "9005",
        ));
        hosts
    }

    #[test]
    fn test_sh_quote_only_escapes_single_quotes() {
        assert_eq!(sh_quote(r"a\b"), r"'a\b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn test_write_scripts_atomic_and_chunked() {
        let scripts = write_scripts("abc");
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].contains("> '/etc/korgi/.dynamic.yml.tmp'"));
        assert!(
            scripts[0].ends_with("mv -f '/etc/korgi/.dynamic.yml.tmp' '/etc/korgi/dynamic.yml'")
        );

        let big = "é".repeat(MAX_CHUNK_BYTES); // 2 bytes per char
        let scripts = write_scripts(&big);
        assert_eq!(scripts.len(), 2);
        assert!(scripts[1].contains(">> "));
        assert!(!scripts[0].contains("mv -f"));
        assert!(scripts[1].contains("mv -f"));
    }

    #[tokio::test]
    async fn test_sync_writes_exact_content_to_every_traefik_host() {
        let config = config_with_traefik();
        let hosts = hosts();
        sync_traefik_config(&config, &hosts).await.unwrap();

        for lb in ["lb1", "lb2"] {
            let written = hosts
                .get(lb)
                .unwrap()
                .file(TRAEFIK_CONTAINER_NAME, CONFIG_FILE)
                .expect("config written");
            assert!(written.contains("http://10.0.0.1:9005"));
            // Backslash in the rule is not doubled; quote survives the shell.
            assert!(written.contains(r"Path(`/a\\b`)"), "{}", written);
            assert!(written.contains("it's.example.com"));
            assert!(
                hosts
                    .get(lb)
                    .unwrap()
                    .file(TRAEFIK_CONTAINER_NAME, TEMP_FILE)
                    .is_none(),
                "temp file renamed away"
            );
        }
    }

    #[tokio::test]
    async fn test_sync_fails_when_traefik_missing_on_any_host() {
        let config = config_with_traefik();
        let hosts = hosts();
        hosts
            .get("lb2")
            .unwrap()
            .set_inspect_error(TRAEFIK_CONTAINER_NAME);
        let err = sync_traefik_config(&config, &hosts).await.unwrap_err();
        assert!(format!("{:#}", err).contains("lb2"), "{:#}", err);
        // Nothing written anywhere: hosts are checked before writing.
        assert!(
            hosts
                .get("lb1")
                .unwrap()
                .file(TRAEFIK_CONTAINER_NAME, CONFIG_FILE)
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_sync_fails_when_traefik_stopped() {
        let config = config_with_traefik();
        let hosts = hosts();
        hosts.get("lb1").unwrap().set_container_running(false);
        assert!(sync_traefik_config(&config, &hosts).await.is_err());
    }

    #[tokio::test]
    async fn test_sync_fails_when_traefik_host_unreachable() {
        let config = config_with_traefik();
        let mut hosts = hosts();
        hosts.remove("lb2");
        assert!(sync_traefik_config(&config, &hosts).await.is_err());
    }

    #[tokio::test]
    async fn test_sync_fails_on_exec_error() {
        let config = config_with_traefik();
        let hosts = hosts();
        hosts.get("lb2").unwrap().set_exec_error("exit status 1");
        assert!(sync_traefik_config(&config, &hosts).await.is_err());
    }

    #[tokio::test]
    async fn test_sync_fails_when_read_back_differs() {
        let config = config_with_traefik();
        let hosts = hosts();
        hosts.get("lb1").unwrap().set_exec_read_override("stale");
        let err = sync_traefik_config(&config, &hosts).await.unwrap_err();
        assert!(format!("{:#}", err).contains("does not match"));
    }

    #[tokio::test]
    async fn test_sync_excluding_leaves_out_containers() {
        let config = config_with_traefik();
        let hosts = hosts();
        let exclude: HashSet<String> = ["c1".to_string()].into();
        sync_traefik_config_excluding(&config, &hosts, &exclude)
            .await
            .unwrap();
        let written = hosts
            .get("lb1")
            .unwrap()
            .file(TRAEFIK_CONTAINER_NAME, CONFIG_FILE)
            .unwrap();
        assert!(!written.contains("9005"));
    }
}
