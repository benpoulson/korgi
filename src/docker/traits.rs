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
