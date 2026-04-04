use anyhow::Result;
use std::collections::HashMap;

use crate::docker::containers::KorgiContainer;
use crate::docker::host::DockerHost;
use crate::docker::labels;

/// Live state of all korgi-managed containers across hosts.
#[derive(Debug)]
pub struct LiveState {
    pub containers: Vec<KorgiContainer>,
}

impl LiveState {
    /// Query all hosts for containers belonging to the given project.
    pub async fn query(
        docker_hosts: &HashMap<String, DockerHost>,
        project: &str,
    ) -> Result<Self> {
        let mut all_containers = Vec::new();
        let filters = labels::project_filter(project);

        for (host_name, docker) in docker_hosts {
            let summaries = docker.list_containers(filters.clone(), true).await?;
            for summary in &summaries {
                if let Some(container) = KorgiContainer::from_summary(summary, host_name) {
                    all_containers.push(container);
                }
            }
        }

        Ok(Self {
            containers: all_containers,
        })
    }

    /// Get containers for a specific service.
    pub fn service_containers(&self, service: &str) -> Vec<&KorgiContainer> {
        self.containers
            .iter()
            .filter(|c| c.service == service)
            .collect()
    }

    /// Get running containers for a specific service.
    pub fn running_service_containers(&self, service: &str) -> Vec<&KorgiContainer> {
        self.containers
            .iter()
            .filter(|c| c.service == service && c.state == "running")
            .collect()
    }

    /// Get the current (highest) generation for a service.
    pub fn current_generation(&self, service: &str) -> Option<u64> {
        self.service_containers(service)
            .iter()
            .map(|c| c.generation)
            .max()
    }

    /// Get the next generation number for a service.
    pub fn next_generation(&self, service: &str) -> u64 {
        self.current_generation(service).map(|g| g + 1).unwrap_or(1)
    }

    /// Get containers for a specific service and generation.
    pub fn generation_containers(
        &self,
        service: &str,
        generation: u64,
    ) -> Vec<&KorgiContainer> {
        self.containers
            .iter()
            .filter(|c| c.service == service && c.generation == generation)
            .collect()
    }

    /// Get all unique service names.
    pub fn services(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .containers
            .iter()
            .map(|c| c.service.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        names.sort();
        names
    }

    /// Find the most recent stopped generation for rollback.
    pub fn rollback_generation(&self, service: &str) -> Option<u64> {
        let current = self.current_generation(service)?;
        // Find the highest generation below current that has stopped containers
        self.service_containers(service)
            .iter()
            .filter(|c| c.generation < current && c.state != "running")
            .map(|c| c.generation)
            .max()
    }
}
