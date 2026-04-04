/// Mock DockerHost for testing orchestrator logic.
/// Records all operations and returns configurable responses.
#[cfg(test)]
pub mod tests {
    use anyhow::Result;
    use async_trait::async_trait;
    use bollard::auth::DockerCredentials;
    use bollard::models::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use crate::docker::traits::DockerHostApi;

    #[derive(Debug, Clone, PartialEq)]
    pub enum DockerCall {
        ListContainers { all: bool },
        PullImage { image: String },
        CreateContainer { name: String },
        StartContainer { id: String },
        StopContainer { id: String, timeout: i64 },
        RemoveContainer { id: String, force: bool },
        InspectContainer { id: String },
        ImageExists { image: String },
        EnsureNetwork { name: String },
    }

    pub struct MockDockerHost {
        pub name: String,
        pub calls: Arc<Mutex<Vec<DockerCall>>>,
        pub containers: Arc<Mutex<Vec<ContainerSummary>>>,
        /// Container ID counter for create_container
        pub id_counter: Arc<Mutex<u32>>,
        /// Health status returned by inspect_container
        pub health_status: Arc<Mutex<Option<HealthStatusEnum>>>,
        /// Whether container is running (for inspect)
        pub container_running: Arc<Mutex<bool>>,
        /// Images that "exist" on this host
        pub existing_images: Arc<Mutex<Vec<String>>>,
        /// If set, pull_image will fail with this error
        pub pull_error: Arc<Mutex<Option<String>>>,
        /// If set, create_container will fail with this error
        pub create_error: Arc<Mutex<Option<String>>>,
        /// If set, start_container will fail with this error
        pub start_error: Arc<Mutex<Option<String>>>,
    }

    impl MockDockerHost {
        pub fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                calls: Arc::new(Mutex::new(Vec::new())),
                containers: Arc::new(Mutex::new(Vec::new())),
                id_counter: Arc::new(Mutex::new(0)),
                health_status: Arc::new(Mutex::new(Some(HealthStatusEnum::HEALTHY))),
                container_running: Arc::new(Mutex::new(true)),
                existing_images: Arc::new(Mutex::new(Vec::new())),
                pull_error: Arc::new(Mutex::new(None)),
                create_error: Arc::new(Mutex::new(None)),
                start_error: Arc::new(Mutex::new(None)),
            }
        }

        pub fn get_calls(&self) -> Vec<DockerCall> {
            self.calls.lock().unwrap().clone()
        }

        pub fn add_container(&self, summary: ContainerSummary) {
            self.containers.lock().unwrap().push(summary);
        }

        pub fn set_health_status(&self, status: Option<HealthStatusEnum>) {
            *self.health_status.lock().unwrap() = status;
        }

        pub fn set_container_running(&self, running: bool) {
            *self.container_running.lock().unwrap() = running;
        }

        pub fn add_existing_image(&self, image: &str) {
            self.existing_images.lock().unwrap().push(image.to_string());
        }

        pub fn set_pull_error(&self, err: &str) {
            *self.pull_error.lock().unwrap() = Some(err.to_string());
        }

        pub fn set_create_error(&self, err: &str) {
            *self.create_error.lock().unwrap() = Some(err.to_string());
        }
    }

    /// Build a ContainerSummary matching korgi labels.
    pub fn mock_container_summary(
        id: &str,
        name: &str,
        project: &str,
        service: &str,
        generation: u64,
        instance: u32,
        image: &str,
        state: ContainerSummaryStateEnum,
        status: &str,
    ) -> ContainerSummary {
        let mut labels = HashMap::new();
        labels.insert("korgi.project".to_string(), project.to_string());
        labels.insert("korgi.service".to_string(), service.to_string());
        labels.insert("korgi.generation".to_string(), generation.to_string());
        labels.insert("korgi.instance".to_string(), instance.to_string());
        labels.insert("korgi.image".to_string(), image.to_string());

        ContainerSummary {
            id: Some(id.to_string()),
            names: Some(vec![format!("/{}", name)]),
            image: Some(image.to_string()),
            labels: Some(labels),
            state: Some(state),
            status: Some(status.to_string()),
            ..Default::default()
        }
    }

    #[async_trait]
    impl DockerHostApi for MockDockerHost {
        fn host_name(&self) -> &str {
            &self.name
        }

        async fn list_containers(
            &self,
            _filters: HashMap<String, Vec<String>>,
            all: bool,
        ) -> Result<Vec<ContainerSummary>> {
            self.calls.lock().unwrap().push(DockerCall::ListContainers { all });
            let containers = self.containers.lock().unwrap().clone();
            if all {
                Ok(containers)
            } else {
                Ok(containers.into_iter().filter(|c| {
                    c.state == Some(ContainerSummaryStateEnum::RUNNING)
                }).collect())
            }
        }

        async fn pull_image(&self, image: &str, _auth: Option<DockerCredentials>) -> Result<()> {
            self.calls.lock().unwrap().push(DockerCall::PullImage {
                image: image.to_string(),
            });
            if let Some(err) = self.pull_error.lock().unwrap().as_ref() {
                anyhow::bail!("{}", err);
            }
            self.existing_images.lock().unwrap().push(image.to_string());
            Ok(())
        }

        async fn create_container(
            &self,
            name: &str,
            _config: ContainerCreateBody,
        ) -> Result<String> {
            self.calls.lock().unwrap().push(DockerCall::CreateContainer {
                name: name.to_string(),
            });
            if let Some(err) = self.create_error.lock().unwrap().as_ref() {
                anyhow::bail!("{}", err);
            }
            let mut counter = self.id_counter.lock().unwrap();
            *counter += 1;
            Ok(format!("container-{}", counter))
        }

        async fn start_container(&self, id: &str) -> Result<()> {
            self.calls.lock().unwrap().push(DockerCall::StartContainer {
                id: id.to_string(),
            });
            if let Some(err) = self.start_error.lock().unwrap().as_ref() {
                anyhow::bail!("{}", err);
            }
            Ok(())
        }

        async fn stop_container(&self, id: &str, timeout_secs: i64) -> Result<()> {
            self.calls.lock().unwrap().push(DockerCall::StopContainer {
                id: id.to_string(),
                timeout: timeout_secs,
            });
            Ok(())
        }

        async fn remove_container(&self, id: &str, force: bool) -> Result<()> {
            self.calls.lock().unwrap().push(DockerCall::RemoveContainer {
                id: id.to_string(),
                force,
            });
            Ok(())
        }

        async fn inspect_container(&self, id: &str) -> Result<ContainerInspectResponse> {
            self.calls.lock().unwrap().push(DockerCall::InspectContainer {
                id: id.to_string(),
            });
            let running = *self.container_running.lock().unwrap();
            let health_status = self.health_status.lock().unwrap().clone();

            let health = health_status.map(|status| Health {
                status: Some(status),
                log: None,
                failing_streak: None,
            });

            Ok(ContainerInspectResponse {
                state: Some(ContainerState {
                    running: Some(running),
                    health,
                    ..Default::default()
                }),
                ..Default::default()
            })
        }

        async fn image_exists(&self, image: &str) -> Result<bool> {
            self.calls.lock().unwrap().push(DockerCall::ImageExists {
                image: image.to_string(),
            });
            Ok(self.existing_images.lock().unwrap().contains(&image.to_string()))
        }

        async fn ensure_network(&self, name: &str) -> Result<()> {
            self.calls.lock().unwrap().push(DockerCall::EnsureNetwork {
                name: name.to_string(),
            });
            Ok(())
        }
    }
}
