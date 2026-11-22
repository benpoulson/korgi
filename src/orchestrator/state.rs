use anyhow::Result;
use std::collections::HashMap;

use crate::docker::containers::{self, KorgiContainer};
use crate::docker::labels;
use crate::docker::traits::DockerHostApi;

/// Live state of all korgi-managed containers across hosts.
#[derive(Debug)]
pub struct LiveState {
    pub containers: Vec<KorgiContainer>,
}

impl LiveState {
    /// Query all hosts for containers belonging to the given project.
    pub async fn query<D: DockerHostApi>(
        docker_hosts: &HashMap<String, D>,
        project: &str,
    ) -> Result<Self> {
        let mut all_containers = Vec::new();
        let filters = labels::project_filter(project);

        for (host_name, docker) in docker_hosts {
            let summaries = docker.list_containers(filters.clone(), true).await?;
            for summary in &summaries {
                if let Some(mut container) = KorgiContainer::from_summary(summary, host_name) {
                    if !container.host_port_known {
                        // Created by an older korgi without the host_port label and not
                        // currently publishing: recover the port from its bindings.
                        let inspect = docker.inspect_container(&container.id).await?;
                        container.host_port =
                            containers::host_port_from_bindings(inspect.host_config.as_ref());
                        container.host_port_known = true;
                    }
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

    /// The generation currently serving: the highest generation that has at least
    /// one running container. Stopped generations (rollback targets, drained
    /// generations) never count, even when their number is higher.
    pub fn current_generation(&self, service: &str) -> Option<u64> {
        self.running_service_containers(service)
            .iter()
            .map(|c| c.generation)
            .max()
    }

    /// Highest generation number that exists for a service, running or not.
    pub fn max_generation(&self, service: &str) -> Option<u64> {
        self.service_containers(service)
            .iter()
            .map(|c| c.generation)
            .max()
    }

    /// Get the next generation number for a service. Always above every existing
    /// generation (stopped ones included) so container names never clash.
    pub fn next_generation(&self, service: &str) -> u64 {
        self.max_generation(service).map(|g| g + 1).unwrap_or(1)
    }

    /// Generations of a service that have running containers, ascending.
    pub fn running_generations(&self, service: &str) -> Vec<u64> {
        let mut gens: Vec<u64> = self
            .running_service_containers(service)
            .iter()
            .map(|c| c.generation)
            .collect();
        gens.sort_unstable();
        gens.dedup();
        gens
    }

    /// Get containers for a specific service and generation.
    pub fn generation_containers(&self, service: &str, generation: u64) -> Vec<&KorgiContainer> {
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

    /// Rollback target: the highest generation below the current one that has
    /// stopped containers.
    pub fn rollback_generation(&self, service: &str) -> Option<u64> {
        let current = self.current_generation(service)?;
        self.service_containers(service)
            .iter()
            .filter(|c| c.generation < current && !c.is_running())
            .map(|c| c.generation)
            .max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::containers::KorgiContainer;

    fn make_container(
        service: &str,
        generation: u64,
        instance: u32,
        host: &str,
        state: &str,
    ) -> KorgiContainer {
        KorgiContainer {
            id: format!("{}-g{}-{}", service, generation, instance),
            name: format!("korgi-app-{}-g{}-{}", service, generation, instance),
            host_name: host.to_string(),
            service: service.to_string(),
            generation,
            instance,
            image: format!("{}:v{}", service, generation),
            state: state.to_string(),
            status: if state == "running" {
                "Up 5 minutes".to_string()
            } else {
                "Exited (0)".to_string()
            },
            health: None,
            host_port: None,
            host_port_known: true,
        }
    }

    fn empty_state() -> LiveState {
        LiveState { containers: vec![] }
    }

    fn sample_state() -> LiveState {
        LiveState {
            containers: vec![
                // api generation 3 (stopped -- old)
                make_container("api", 3, 0, "web1", "exited"),
                make_container("api", 3, 1, "web2", "exited"),
                // api generation 4 (running -- current)
                make_container("api", 4, 0, "web1", "running"),
                make_container("api", 4, 1, "web2", "running"),
                make_container("api", 4, 2, "web1", "running"),
                // worker generation 2 (running)
                make_container("worker", 2, 0, "web1", "running"),
                make_container("worker", 2, 1, "web2", "running"),
            ],
        }
    }

    // --- services ---

    #[test]
    fn test_services_empty() {
        let state = empty_state();
        assert!(state.services().is_empty());
    }

    #[test]
    fn test_services_lists_unique_sorted() {
        let state = sample_state();
        let services = state.services();
        assert_eq!(services, vec!["api", "worker"]);
    }

    // --- service_containers ---

    #[test]
    fn test_service_containers_filters_correctly() {
        let state = sample_state();
        let api = state.service_containers("api");
        assert_eq!(api.len(), 5); // 2 stopped + 3 running
        assert!(api.iter().all(|c| c.service == "api"));
    }

    #[test]
    fn test_service_containers_nonexistent() {
        let state = sample_state();
        let empty = state.service_containers("nonexistent");
        assert!(empty.is_empty());
    }

    // --- running_service_containers ---

    #[test]
    fn test_running_service_containers() {
        let state = sample_state();
        let running = state.running_service_containers("api");
        assert_eq!(running.len(), 3);
        assert!(running.iter().all(|c| c.state == "running"));
    }

    #[test]
    fn test_running_service_containers_all_stopped() {
        let state = LiveState {
            containers: vec![make_container("api", 1, 0, "web1", "exited")],
        };
        assert!(state.running_service_containers("api").is_empty());
    }

    // --- current_generation ---

    #[test]
    fn test_current_generation() {
        let state = sample_state();
        assert_eq!(state.current_generation("api"), Some(4));
        assert_eq!(state.current_generation("worker"), Some(2));
    }

    #[test]
    fn test_current_generation_ignores_stopped() {
        // A stopped higher generation (e.g. the one that was rolled back from) is
        // not current; the running one is.
        let state = LiveState {
            containers: vec![
                make_container("api", 5, 0, "web1", "exited"),
                make_container("api", 4, 0, "web1", "running"),
                make_container("api", 3, 0, "web1", "exited"),
            ],
        };
        assert_eq!(state.current_generation("api"), Some(4));
        // next_generation still clears every existing name
        assert_eq!(state.next_generation("api"), 6);
        assert_eq!(state.rollback_generation("api"), Some(3));
    }

    #[test]
    fn test_current_generation_all_stopped() {
        let state = LiveState {
            containers: vec![
                make_container("api", 5, 0, "web1", "exited"),
                make_container("api", 3, 0, "web1", "exited"),
            ],
        };
        assert_eq!(state.current_generation("api"), None);
        assert_eq!(state.next_generation("api"), 6);
        assert_eq!(state.rollback_generation("api"), None);
    }

    #[test]
    fn test_state_after_two_rollbacks() {
        // gen 3 deployed, rolled back to 2, rolled back again to 1:
        // gen 1 running, gens 2 and 3 stopped.
        let state = LiveState {
            containers: vec![
                make_container("api", 1, 0, "web1", "running"),
                make_container("api", 2, 0, "web1", "exited"),
                make_container("api", 3, 0, "web1", "exited"),
            ],
        };
        assert_eq!(state.current_generation("api"), Some(1));
        assert_eq!(state.rollback_generation("api"), None);
        assert_eq!(state.running_generations("api"), vec![1]);
        assert_eq!(state.next_generation("api"), 4);
    }

    #[test]
    fn test_current_generation_nonexistent() {
        let state = sample_state();
        assert_eq!(state.current_generation("nonexistent"), None);
    }

    #[test]
    fn test_current_generation_empty() {
        let state = empty_state();
        assert_eq!(state.current_generation("api"), None);
    }

    // --- next_generation ---

    #[test]
    fn test_next_generation() {
        let state = sample_state();
        assert_eq!(state.next_generation("api"), 5);
        assert_eq!(state.next_generation("worker"), 3);
    }

    #[test]
    fn test_next_generation_no_existing() {
        let state = empty_state();
        assert_eq!(state.next_generation("api"), 1);
    }

    #[test]
    fn test_next_generation_new_service() {
        let state = sample_state();
        assert_eq!(state.next_generation("new-service"), 1);
    }

    // --- generation_containers ---

    #[test]
    fn test_generation_containers() {
        let state = sample_state();
        let gen4 = state.generation_containers("api", 4);
        assert_eq!(gen4.len(), 3);
        assert!(gen4.iter().all(|c| c.generation == 4));
    }

    #[test]
    fn test_generation_containers_old_gen() {
        let state = sample_state();
        let gen3 = state.generation_containers("api", 3);
        assert_eq!(gen3.len(), 2);
        assert!(gen3.iter().all(|c| c.state == "exited"));
    }

    #[test]
    fn test_generation_containers_nonexistent() {
        let state = sample_state();
        let gen99 = state.generation_containers("api", 99);
        assert!(gen99.is_empty());
    }

    // --- rollback_generation ---

    #[test]
    fn test_rollback_generation() {
        let state = sample_state();
        // api has gen 3 (stopped) and gen 4 (running) -- rollback to 3
        assert_eq!(state.rollback_generation("api"), Some(3));
    }

    #[test]
    fn test_rollback_generation_no_stopped() {
        let state = LiveState {
            containers: vec![
                make_container("api", 4, 0, "web1", "running"),
                make_container("api", 4, 1, "web2", "running"),
            ],
        };
        assert_eq!(state.rollback_generation("api"), None);
    }

    #[test]
    fn test_rollback_generation_multiple_stopped() {
        let state = LiveState {
            containers: vec![
                make_container("api", 1, 0, "web1", "exited"),
                make_container("api", 2, 0, "web1", "exited"),
                make_container("api", 3, 0, "web1", "exited"),
                make_container("api", 4, 0, "web1", "running"),
            ],
        };
        // Should pick the most recent stopped gen (3)
        assert_eq!(state.rollback_generation("api"), Some(3));
    }

    #[test]
    fn test_rollback_generation_nonexistent_service() {
        let state = sample_state();
        assert_eq!(state.rollback_generation("nonexistent"), None);
    }

    #[test]
    fn test_rollback_generation_single_gen() {
        let state = LiveState {
            containers: vec![make_container("api", 1, 0, "web1", "running")],
        };
        // Only one generation, nothing to roll back to
        assert_eq!(state.rollback_generation("api"), None);
    }

    // --- host distribution ---

    #[test]
    fn test_containers_track_host_names() {
        let state = sample_state();
        let gen4 = state.generation_containers("api", 4);
        let hosts: Vec<&str> = gen4.iter().map(|c| c.host_name.as_str()).collect();
        assert!(hosts.contains(&"web1"));
        assert!(hosts.contains(&"web2"));
    }

    // --- mixed service state ---

    #[test]
    fn test_services_independent() {
        let state = sample_state();
        // api and worker generations are independent
        assert_eq!(state.current_generation("api"), Some(4));
        assert_eq!(state.current_generation("worker"), Some(2));
        assert_eq!(state.next_generation("api"), 5);
        assert_eq!(state.next_generation("worker"), 3);
    }

    #[tokio::test]
    async fn test_query_recovers_port_of_legacy_container_from_bindings() {
        use crate::docker::mock::tests::*;
        use bollard::models::*;

        let host = MockDockerHost::new("web1");
        // Created by an older korgi: no korgi.host_port label, stopped.
        host.add_container(mock_container_summary(
            "legacy",
            "korgi-app-api-g1-0",
            "app",
            "api",
            1,
            0,
            "api:v1",
            ContainerSummaryStateEnum::EXITED,
            "Exited",
        ));
        let mut bindings = std::collections::HashMap::new();
        bindings.insert(
            "8080/tcp".to_string(),
            Some(vec![PortBinding {
                host_ip: Some("10.0.0.1".into()),
                host_port: Some("9017".into()),
            }]),
        );
        host.set_inspect_response(
            "legacy",
            ContainerInspectResponse {
                host_config: Some(HostConfig {
                    port_bindings: Some(bindings),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        // New container with the label: no inspect needed.
        host.add_container(with_label(
            mock_container_summary(
                "new",
                "korgi-app-api-g2-0",
                "app",
                "api",
                2,
                0,
                "api:v2",
                ContainerSummaryStateEnum::RUNNING,
                "Up",
            ),
            "korgi.host_port",
            "9020",
        ));
        let mut hosts = std::collections::HashMap::new();
        hosts.insert("web1".to_string(), host);

        let state = LiveState::query(&hosts, "app").await.unwrap();
        let legacy = state.containers.iter().find(|c| c.id == "legacy").unwrap();
        let new = state.containers.iter().find(|c| c.id == "new").unwrap();
        assert_eq!(legacy.host_port, Some(9017));
        assert_eq!(new.host_port, Some(9020));
        let inspected: Vec<_> = hosts["web1"]
            .get_calls()
            .into_iter()
            .filter(|c| matches!(c, DockerCall::InspectContainer { .. }))
            .collect();
        assert_eq!(inspected.len(), 1, "only the legacy container is inspected");
    }
}
