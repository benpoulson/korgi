use anyhow::Result;
use async_trait::async_trait;
use bollard::auth::DockerCredentials;
use bollard::models::{ContainerCreateBody, ContainerInspectResponse, ContainerSummary};
use std::collections::HashMap;

/// Abstraction over Docker operations for a single host.
/// Enables testing orchestrator logic without real Docker/SSH connections.
#[async_trait]
pub trait DockerHostApi: Send + Sync {
    fn host_name(&self) -> &str;

    async fn list_containers(
        &self,
        filters: HashMap<String, Vec<String>>,
        all: bool,
    ) -> Result<Vec<ContainerSummary>>;

    async fn pull_image(&self, image: &str, auth: Option<DockerCredentials>) -> Result<()>;

    async fn create_container(&self, name: &str, config: ContainerCreateBody) -> Result<String>;

    async fn start_container(&self, id: &str) -> Result<()>;

    async fn stop_container(&self, id: &str, timeout_secs: i64) -> Result<()>;

    async fn remove_container(&self, id: &str, force: bool) -> Result<()>;

    async fn inspect_container(&self, id: &str) -> Result<ContainerInspectResponse>;

    async fn image_exists(&self, image: &str) -> Result<bool>;

    async fn ensure_network(&self, name: &str) -> Result<()>;
}

/// Running commands inside containers (used to write Traefik's dynamic config).
///
/// Kept separate from [`DockerHostApi`] so the orchestrator can be tested with the
/// mock host. `exec_in_container` must return `Err` when the command exits non-zero.
#[async_trait]
pub trait DockerExecApi: DockerHostApi {
    async fn exec_in_container(&self, container: &str, cmd: &[&str]) -> Result<String>;
}

#[async_trait]
impl DockerExecApi for super::host::DockerHost {
    async fn exec_in_container(&self, container: &str, cmd: &[&str]) -> Result<String> {
        super::host::DockerHost::exec_in_container(self, container, cmd).await
    }
}
