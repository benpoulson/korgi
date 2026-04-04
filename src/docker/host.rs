use anyhow::{Context, Result};
use async_trait::async_trait;
use bollard::models::{ContainerCreateBody, NetworkCreateRequest};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, InspectNetworkOptions,
    ListContainersOptions, LogsOptions, RemoveContainerOptions, StopContainerOptions,
};
use bollard::Docker;
use futures::StreamExt;
use std::collections::HashMap;
use tracing::{debug, instrument};

use crate::config::types::HostConfig;
use super::traits::DockerHostApi;

/// Docker client connected to a remote host via SSH.
pub struct DockerHost {
    client: Docker,
    pub host_name: String,
}

impl DockerHost {
    /// Connect to Docker on a remote host via SSH.
    #[instrument(skip_all, fields(host = %host.name))]
    pub async fn connect(host: &HostConfig) -> Result<Self> {
        let ssh_url = if host.port != 22 {
            format!("ssh://{}@{}:{}", host.user, host.ssh_address(), host.port)
        } else {
            format!("ssh://{}@{}", host.user, host.ssh_address())
        };
        debug!("Connecting Docker client via {}", ssh_url);

        let key_path = host.ssh_key.as_ref().map(|k| expand_tilde(k));

        let client = Docker::connect_with_ssh(
            &ssh_url,
            120,
            bollard::API_DEFAULT_VERSION,
            key_path,
        )
        .with_context(|| format!("Failed to connect Docker on {}", host.name))?;

        // Verify connection
        let _: String = client
            .ping()
            .await
            .with_context(|| format!("Docker ping failed on {}", host.name))?;

        debug!("Docker connected on {}", host.name);
        Ok(Self {
            client,
            host_name: host.name.clone(),
        })
    }

    /// Connect to a local Docker instance.
    pub fn connect_local() -> Result<Self> {
        let client = Docker::connect_with_local_defaults()
            .context("Failed to connect to local Docker")?;
        Ok(Self {
            client,
            host_name: "local".to_string(),
        })
    }

    /// List containers matching the given label filters.
    pub async fn list_containers(
        &self,
        filters: HashMap<String, Vec<String>>,
        all: bool,
    ) -> Result<Vec<bollard::models::ContainerSummary>> {
        let options = ListContainersOptions {
            all,
            filters: Some(filters),
            ..Default::default()
        };
        self.client
            .list_containers(Some(options))
            .await
            .with_context(|| format!("Failed to list containers on {}", self.host_name))
    }

    /// Pull an image on the remote host.
    #[instrument(skip(self), fields(host = %self.host_name))]
    pub async fn pull_image(&self, image: &str, auth: Option<bollard::auth::DockerCredentials>) -> Result<()> {
        debug!("Pulling image: {}", image);

        let (repo, tag) = parse_image_ref(image);
        let options = CreateImageOptions {
            from_image: Some(repo.to_string()),
            tag: Some(tag.to_string()),
            ..Default::default()
        };

        let mut stream = self.client.create_image(Some(options), None, auth);
        while let Some(result) = stream.next().await {
            let _info: bollard::models::CreateImageInfo = result
                .with_context(|| format!("Failed to pull {} on {}", image, self.host_name))?;
        }
        Ok(())
    }

    /// Create and return a container (does not start it).
    pub async fn create_container(
        &self,
        name: &str,
        config: ContainerCreateBody,
    ) -> Result<String> {
        let options = CreateContainerOptions {
            name: Some(name.to_string()),
            ..Default::default()
        };
        let response = self
            .client
            .create_container(Some(options), config)
            .await
            .with_context(|| {
                format!("Failed to create container {} on {}", name, self.host_name)
            })?;
        Ok(response.id)
    }

    /// Start a container by ID or name.
    pub async fn start_container(&self, id: &str) -> Result<()> {
        self.client
            .start_container(id, None::<bollard::query_parameters::StartContainerOptions>)
            .await
            .with_context(|| format!("Failed to start container {} on {}", id, self.host_name))
    }

    /// Stop a container with a timeout.
    pub async fn stop_container(&self, id: &str, timeout_secs: i64) -> Result<()> {
        let options = StopContainerOptions {
            t: Some(timeout_secs as i32),
            signal: None,
        };
        self.client
            .stop_container(id, Some(options))
            .await
            .with_context(|| format!("Failed to stop container {} on {}", id, self.host_name))
    }

    /// Remove a container.
    pub async fn remove_container(&self, id: &str, force: bool) -> Result<()> {
        let options = RemoveContainerOptions {
            force,
            ..Default::default()
        };
        self.client
            .remove_container(id, Some(options))
            .await
            .with_context(|| format!("Failed to remove container {} on {}", id, self.host_name))
    }

    /// Inspect a container for details (health status, state, etc.)
    pub async fn inspect_container(
        &self,
        id: &str,
    ) -> Result<bollard::models::ContainerInspectResponse> {
        self.client
            .inspect_container(id, None::<InspectContainerOptions>)
            .await
            .with_context(|| format!("Failed to inspect container {} on {}", id, self.host_name))
    }

    /// Check if an image exists locally on the host.
    pub async fn image_exists(&self, image: &str) -> Result<bool> {
        match self.client.inspect_image(image).await {
            Ok(_) => Ok(true),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(e).context(format!(
                "Failed to check image {} on {}",
                image, self.host_name
            )),
        }
    }

    /// Get container logs.
    pub fn logs(
        &self,
        id: &str,
        follow: bool,
        tail: &str,
    ) -> impl futures::Stream<Item = Result<bollard::container::LogOutput, bollard::errors::Error>> + '_
    {
        let options = LogsOptions {
            follow,
            stdout: true,
            stderr: true,
            tail: tail.to_string(),
            ..Default::default()
        };
        self.client.logs(id, Some(options))
    }

    /// Ensure a Docker network exists, creating it if not.
    pub async fn ensure_network(&self, name: &str) -> Result<()> {
        match self.client.inspect_network(name, None::<InspectNetworkOptions>).await {
            Ok(_) => {
                debug!("Network {} already exists on {}", name, self.host_name);
                Ok(())
            }
            Err(_) => {
                debug!("Creating network {} on {}", name, self.host_name);
                let options = NetworkCreateRequest {
                    name: name.to_string(),
                    driver: Some("bridge".to_string()),
                    ..Default::default()
                };
                self.client
                    .create_network(options)
                    .await
                    .with_context(|| {
                        format!("Failed to create network {} on {}", name, self.host_name)
                    })?;
                Ok(())
            }
        }
    }

    /// Get the underlying bollard client reference.
    pub fn client(&self) -> &Docker {
        &self.client
    }
}

#[async_trait]
impl DockerHostApi for DockerHost {
    fn host_name(&self) -> &str {
        &self.host_name
    }

    async fn list_containers(
        &self,
        filters: HashMap<String, Vec<String>>,
        all: bool,
    ) -> Result<Vec<bollard::models::ContainerSummary>> {
        DockerHost::list_containers(self, filters, all).await
    }

    async fn pull_image(&self, image: &str, auth: Option<bollard::auth::DockerCredentials>) -> Result<()> {
        DockerHost::pull_image(self, image, auth).await
    }

    async fn create_container(
        &self,
        name: &str,
        config: ContainerCreateBody,
    ) -> Result<String> {
        DockerHost::create_container(self, name, config).await
    }

    async fn start_container(&self, id: &str) -> Result<()> {
        DockerHost::start_container(self, id).await
    }

    async fn stop_container(&self, id: &str, timeout_secs: i64) -> Result<()> {
        DockerHost::stop_container(self, id, timeout_secs).await
    }

    async fn remove_container(&self, id: &str, force: bool) -> Result<()> {
        DockerHost::remove_container(self, id, force).await
    }

    async fn inspect_container(
        &self,
        id: &str,
    ) -> Result<bollard::models::ContainerInspectResponse> {
        DockerHost::inspect_container(self, id).await
    }

    async fn image_exists(&self, image: &str) -> Result<bool> {
        DockerHost::image_exists(self, image).await
    }

    async fn ensure_network(&self, name: &str) -> Result<()> {
        DockerHost::ensure_network(self, name).await
    }
}

/// Parse an image reference into (repository, tag).
fn parse_image_ref(image: &str) -> (&str, &str) {
    if let Some(colon_pos) = image.rfind(':') {
        let slash_pos = image.rfind('/').unwrap_or(0);
        if colon_pos > slash_pos {
            return (&image[..colon_pos], &image[colon_pos + 1..]);
        }
    }
    (image, "latest")
}

/// Expand ~ to home directory.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{}/{}", home, rest);
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_image_ref_simple() {
        assert_eq!(parse_image_ref("nginx:1.25"), ("nginx", "1.25"));
    }

    #[test]
    fn test_parse_image_ref_no_tag() {
        assert_eq!(parse_image_ref("nginx"), ("nginx", "latest"));
    }

    #[test]
    fn test_parse_image_ref_with_registry() {
        assert_eq!(
            parse_image_ref("ghcr.io/user/repo:v1"),
            ("ghcr.io/user/repo", "v1")
        );
    }

    #[test]
    fn test_parse_image_ref_registry_with_port_no_tag() {
        // registry.example.com:5000/repo -- the :5000 is a port, not a tag
        assert_eq!(
            parse_image_ref("registry.example.com:5000/repo"),
            ("registry.example.com:5000/repo", "latest")
        );
    }

    #[test]
    fn test_parse_image_ref_registry_with_port_and_tag() {
        assert_eq!(
            parse_image_ref("registry.example.com:5000/repo:v2"),
            ("registry.example.com:5000/repo", "v2")
        );
    }

    #[test]
    fn test_parse_image_ref_sha_digest() {
        // sha256 digests use @ not : but let's make sure : in digest doesn't break
        assert_eq!(
            parse_image_ref("myapp:sha-abc123"),
            ("myapp", "sha-abc123")
        );
    }

    #[test]
    fn test_parse_image_ref_nested_path() {
        assert_eq!(
            parse_image_ref("ghcr.io/org/team/app:latest"),
            ("ghcr.io/org/team/app", "latest")
        );
    }

    #[test]
    fn test_expand_tilde_with_home() {
        let result = expand_tilde("~/path/to/key");
        // Should expand to $HOME/path/to/key
        assert!(!result.starts_with("~/"));
        assert!(result.ends_with("path/to/key"));
    }

    #[test]
    fn test_expand_tilde_no_tilde() {
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
    }

    #[test]
    fn test_expand_tilde_just_tilde() {
        // "~" without "/" should not expand (only "~/" does)
        assert_eq!(expand_tilde("~"), "~");
    }

    #[test]
    fn test_expand_tilde_relative() {
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }
}
