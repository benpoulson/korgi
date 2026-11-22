/// Mock DockerHost for testing orchestrator logic.
/// Records all operations and returns configurable responses.
#[cfg(test)]
pub mod tests {
    use anyhow::Result;
    use async_trait::async_trait;
    use bollard::auth::DockerCredentials;
    use bollard::models::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    use crate::docker::traits::{DockerExecApi, DockerHostApi};

    #[derive(Debug, Clone, PartialEq)]
    pub enum DockerCall {
        ListContainers { all: bool },
        PullImage { image: String, with_auth: bool },
        CreateContainer { name: String },
        StartContainer { id: String },
        StopContainer { id: String, timeout: i64 },
        RemoveContainer { id: String, force: bool },
        InspectContainer { id: String },
        ImageExists { image: String },
        EnsureNetwork { name: String },
        Exec { container: String, script: String },
    }

    /// Call log shared by several mock hosts, to assert cross-host ordering.
    pub type SharedLog = Arc<Mutex<Vec<(String, DockerCall)>>>;

    pub struct MockDockerHost {
        pub name: String,
        pub calls: Arc<Mutex<Vec<DockerCall>>>,
        pub shared_log: Option<SharedLog>,
        pub containers: Arc<Mutex<Vec<ContainerSummary>>>,
        /// Container ID counter for create_container
        pub id_counter: Arc<Mutex<u32>>,
        /// Health status returned by inspect_container
        pub health_status: Arc<Mutex<Option<HealthStatusEnum>>>,
        /// Whether container is running (for inspect)
        pub container_running: Arc<Mutex<bool>>,
        /// Restart counts returned by successive inspects (last value repeats).
        pub restart_counts: Arc<Mutex<Vec<i64>>>,
        /// Images that "exist" on this host
        pub existing_images: Arc<Mutex<Vec<String>>>,
        /// If set, pull_image will fail with this error
        pub pull_error: Arc<Mutex<Option<String>>>,
        /// If set, create_container will fail with this error
        pub create_error: Arc<Mutex<Option<String>>>,
        /// If set, create_container fails once this many containers were created.
        pub create_fail_after: Arc<Mutex<Option<u32>>>,
        /// If set, start_container will fail with this error
        pub start_error: Arc<Mutex<Option<String>>>,
        /// Container ids whose stop fails.
        pub stop_errors: Arc<Mutex<HashSet<String>>>,
        /// Container ids/names whose inspect fails (e.g. "korgi-traefik" missing).
        pub inspect_errors: Arc<Mutex<HashSet<String>>>,
        /// Inspect responses keyed by container id
        pub inspect_responses: Arc<Mutex<HashMap<String, ContainerInspectResponse>>>,
        /// Configs passed to create_container, by name.
        pub created_configs: Arc<Mutex<Vec<(String, ContainerCreateBody)>>>,
        /// Files inside containers written through exec: (container, path) -> content.
        pub files: Arc<Mutex<HashMap<(String, String), String>>>,
        /// If set, every exec fails with this error (non-zero exit).
        pub exec_error: Arc<Mutex<Option<String>>>,
        /// If set, `cat` returns this instead of the file content.
        pub exec_read_override: Arc<Mutex<Option<String>>>,
    }

    impl MockDockerHost {
        pub fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                calls: Arc::new(Mutex::new(Vec::new())),
                shared_log: None,
                containers: Arc::new(Mutex::new(Vec::new())),
                id_counter: Arc::new(Mutex::new(0)),
                health_status: Arc::new(Mutex::new(Some(HealthStatusEnum::HEALTHY))),
                container_running: Arc::new(Mutex::new(true)),
                restart_counts: Arc::new(Mutex::new(Vec::new())),
                existing_images: Arc::new(Mutex::new(Vec::new())),
                pull_error: Arc::new(Mutex::new(None)),
                create_error: Arc::new(Mutex::new(None)),
                create_fail_after: Arc::new(Mutex::new(None)),
                start_error: Arc::new(Mutex::new(None)),
                stop_errors: Arc::new(Mutex::new(HashSet::new())),
                inspect_errors: Arc::new(Mutex::new(HashSet::new())),
                inspect_responses: Arc::new(Mutex::new(HashMap::new())),
                created_configs: Arc::new(Mutex::new(Vec::new())),
                files: Arc::new(Mutex::new(HashMap::new())),
                exec_error: Arc::new(Mutex::new(None)),
                exec_read_override: Arc::new(Mutex::new(None)),
            }
        }

        pub fn with_shared_log(mut self, log: &SharedLog) -> Self {
            self.shared_log = Some(log.clone());
            self
        }

        fn record(&self, call: DockerCall) {
            if let Some(log) = &self.shared_log {
                log.lock().unwrap().push((self.name.clone(), call.clone()));
            }
            self.calls.lock().unwrap().push(call);
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

        pub fn set_restart_counts(&self, counts: &[i64]) {
            *self.restart_counts.lock().unwrap() = counts.to_vec();
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

        pub fn set_create_fail_after(&self, n: u32) {
            *self.create_fail_after.lock().unwrap() = Some(n);
        }

        pub fn set_stop_error(&self, id: &str) {
            self.stop_errors.lock().unwrap().insert(id.to_string());
        }

        pub fn set_inspect_error(&self, id: &str) {
            self.inspect_errors.lock().unwrap().insert(id.to_string());
        }

        pub fn set_exec_error(&self, err: &str) {
            *self.exec_error.lock().unwrap() = Some(err.to_string());
        }

        pub fn set_exec_read_override(&self, content: &str) {
            *self.exec_read_override.lock().unwrap() = Some(content.to_string());
        }

        pub fn set_inspect_response(&self, id: &str, response: ContainerInspectResponse) {
            self.inspect_responses
                .lock()
                .unwrap()
                .insert(id.to_string(), response);
        }

        /// Content of a file written into a container via exec.
        pub fn file(&self, container: &str, path: &str) -> Option<String> {
            self.files
                .lock()
                .unwrap()
                .get(&(container.to_string(), path.to_string()))
                .cloned()
        }

        pub fn created_config(&self, name: &str) -> Option<ContainerCreateBody> {
            self.created_configs
                .lock()
                .unwrap()
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, c)| c.clone())
        }

        fn find_container(&self, id_or_name: &str) -> Option<ContainerSummary> {
            self.containers
                .lock()
                .unwrap()
                .iter()
                .find(|c| matches_id_or_name(c, id_or_name))
                .cloned()
        }

        fn set_state(&self, id: &str, state: ContainerSummaryStateEnum) {
            let created = self.created_configs.lock().unwrap().clone();
            let mut containers = self.containers.lock().unwrap();
            if let Some(c) = containers.iter_mut().find(|c| matches_id_or_name(c, id)) {
                c.state = Some(state);
                if state == ContainerSummaryStateEnum::RUNNING {
                    c.status = Some("Up 1 second".to_string());
                    let name = c
                        .names
                        .as_ref()
                        .and_then(|n| n.first())
                        .map(|n| n.trim_start_matches('/').to_string())
                        .unwrap_or_default();
                    let port = created
                        .iter()
                        .find(|(n, _)| *n == name)
                        .and_then(|(_, cfg)| {
                            crate::docker::containers::host_port_from_bindings(
                                cfg.host_config.as_ref(),
                            )
                        });
                    if let Some(port) = port {
                        c.ports = Some(vec![PortSummary {
                            public_port: Some(port),
                            ..Default::default()
                        }]);
                    }
                } else {
                    c.status = Some("Exited (0)".to_string());
                    c.ports = None;
                }
            }
        }
    }

    fn matches_id_or_name(c: &ContainerSummary, id_or_name: &str) -> bool {
        c.id.as_deref() == Some(id_or_name)
            || c.names
                .as_ref()
                .is_some_and(|n| n.iter().any(|n| n.trim_start_matches('/') == id_or_name))
    }

    /// Build a ContainerSummary matching korgi labels.
    #[allow(clippy::too_many_arguments)]
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

    /// Add (or overwrite) a label on a summary.
    pub fn with_label(mut summary: ContainerSummary, key: &str, value: &str) -> ContainerSummary {
        summary
            .labels
            .get_or_insert_with(HashMap::new)
            .insert(key.to_string(), value.to_string());
        summary
    }

    /// Shell-style tokenizer for the subset of `sh -c` scripts korgi generates:
    /// whitespace-separated words, single-quoted strings (with the `'\''` idiom)
    /// and the operators `&&`, `>` and `>>`.
    fn tokenize(script: &str) -> Result<Vec<String>> {
        let mut tokens = Vec::new();
        let mut cur = String::new();
        let mut in_token = false;
        let mut chars = script.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\'' => {
                    in_token = true;
                    loop {
                        match chars.next() {
                            Some('\'') => break,
                            Some(ch) => cur.push(ch),
                            None => anyhow::bail!("unterminated quote in {}", script),
                        }
                    }
                }
                '\\' => {
                    in_token = true;
                    match chars.next() {
                        Some(ch) => cur.push(ch),
                        None => anyhow::bail!("dangling backslash"),
                    }
                }
                c if c.is_whitespace() => {
                    if in_token {
                        tokens.push(std::mem::take(&mut cur));
                        in_token = false;
                    }
                }
                _ => {
                    in_token = true;
                    cur.push(c);
                }
            }
        }
        if in_token {
            tokens.push(cur);
        }
        Ok(tokens)
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
            self.record(DockerCall::ListContainers { all });
            let containers = self.containers.lock().unwrap().clone();
            if all {
                Ok(containers)
            } else {
                Ok(containers
                    .into_iter()
                    .filter(|c| c.state == Some(ContainerSummaryStateEnum::RUNNING))
                    .collect())
            }
        }

        async fn pull_image(&self, image: &str, auth: Option<DockerCredentials>) -> Result<()> {
            self.record(DockerCall::PullImage {
                image: image.to_string(),
                with_auth: auth.is_some(),
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
            config: ContainerCreateBody,
        ) -> Result<String> {
            self.record(DockerCall::CreateContainer {
                name: name.to_string(),
            });
            if let Some(err) = self.create_error.lock().unwrap().as_ref() {
                anyhow::bail!("{}", err);
            }
            let mut counter = self.id_counter.lock().unwrap();
            if let Some(limit) = *self.create_fail_after.lock().unwrap()
                && *counter >= limit
            {
                anyhow::bail!("port is already allocated");
            }
            *counter += 1;
            let id = format!("{}-container-{}", self.name, counter);
            self.created_configs
                .lock()
                .unwrap()
                .push((name.to_string(), config.clone()));
            self.containers.lock().unwrap().push(ContainerSummary {
                id: Some(id.clone()),
                names: Some(vec![format!("/{}", name)]),
                image: config.image.clone(),
                labels: config.labels.clone(),
                state: Some(ContainerSummaryStateEnum::CREATED),
                status: Some("Created".to_string()),
                ..Default::default()
            });
            Ok(id)
        }

        async fn start_container(&self, id: &str) -> Result<()> {
            self.record(DockerCall::StartContainer { id: id.to_string() });
            if let Some(err) = self.start_error.lock().unwrap().as_ref() {
                anyhow::bail!("{}", err);
            }
            self.set_state(id, ContainerSummaryStateEnum::RUNNING);
            Ok(())
        }

        async fn stop_container(&self, id: &str, timeout_secs: i64) -> Result<()> {
            self.record(DockerCall::StopContainer {
                id: id.to_string(),
                timeout: timeout_secs,
            });
            if self.stop_errors.lock().unwrap().contains(id) {
                anyhow::bail!("failed to stop {}", id);
            }
            self.set_state(id, ContainerSummaryStateEnum::EXITED);
            Ok(())
        }

        async fn remove_container(&self, id: &str, force: bool) -> Result<()> {
            self.record(DockerCall::RemoveContainer {
                id: id.to_string(),
                force,
            });
            self.containers
                .lock()
                .unwrap()
                .retain(|c| !matches_id_or_name(c, id));
            Ok(())
        }

        async fn inspect_container(&self, id: &str) -> Result<ContainerInspectResponse> {
            self.record(DockerCall::InspectContainer { id: id.to_string() });
            if self.inspect_errors.lock().unwrap().contains(id) {
                anyhow::bail!("No such container: {}", id);
            }
            if let Some(response) = self.inspect_responses.lock().unwrap().get(id).cloned() {
                return Ok(response);
            }
            let listed = self.find_container(id);
            let listed_running = listed.as_ref().is_none_or(|c| {
                c.state != Some(ContainerSummaryStateEnum::EXITED)
                    && c.state != Some(ContainerSummaryStateEnum::CREATED)
            });
            let running = *self.container_running.lock().unwrap() && listed_running;
            let health_status = *self.health_status.lock().unwrap();
            let restart_count = {
                let mut counts = self.restart_counts.lock().unwrap();
                match counts.len() {
                    0 => None,
                    1 => Some(counts[0]),
                    _ => Some(counts.remove(0)),
                }
            };

            let health = health_status.map(|status| Health {
                status: Some(status),
                log: None,
                failing_streak: None,
            });

            let name = listed
                .as_ref()
                .and_then(|c| c.names.as_ref())
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string());
            let created_host_config = name.and_then(|name| {
                self.created_configs
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(n, _)| *n == name)
                    .and_then(|(_, cfg)| cfg.host_config.clone())
            });

            Ok(ContainerInspectResponse {
                restart_count,
                host_config: created_host_config,
                state: Some(ContainerState {
                    running: Some(running),
                    health,
                    ..Default::default()
                }),
                ..Default::default()
            })
        }

        async fn image_exists(&self, image: &str) -> Result<bool> {
            self.record(DockerCall::ImageExists {
                image: image.to_string(),
            });
            Ok(self
                .existing_images
                .lock()
                .unwrap()
                .contains(&image.to_string()))
        }

        async fn ensure_network(&self, name: &str) -> Result<()> {
            self.record(DockerCall::EnsureNetwork {
                name: name.to_string(),
            });
            Ok(())
        }
    }

    #[async_trait]
    impl DockerExecApi for MockDockerHost {
        /// Emulates `sh -c` scripts made of `mkdir -p`, `printf '%s' X > F`,
        /// `printf '%s' X >> F`, `mv -f A B` and `cat F`, joined with `&&`.
        async fn exec_in_container(&self, container: &str, cmd: &[&str]) -> Result<String> {
            let script = match cmd {
                ["sh", "-c", script] => script.to_string(),
                other => other.join(" "),
            };
            self.record(DockerCall::Exec {
                container: container.to_string(),
                script: script.clone(),
            });
            if let Some(err) = self.exec_error.lock().unwrap().as_ref() {
                anyhow::bail!("{}", err);
            }
            if self.inspect_errors.lock().unwrap().contains(container) {
                anyhow::bail!("No such container: {}", container);
            }

            let tokens = tokenize(&script)?;
            let mut output = String::new();
            let mut files = self.files.lock().unwrap();
            for command in tokens.split(|t| t == "&&") {
                let key = |p: &str| (container.to_string(), p.to_string());
                match command {
                    [mkdir, flag, _dir] if mkdir == "mkdir" && flag == "-p" => {}
                    [printf, fmt, content, op, path] if printf == "printf" && fmt == "%s" => {
                        match op.as_str() {
                            ">" => {
                                files.insert(key(path), content.clone());
                            }
                            ">>" => {
                                files.entry(key(path)).or_default().push_str(content);
                            }
                            other => anyhow::bail!("unsupported redirect {}", other),
                        }
                    }
                    [mv, flag, from, to] if mv == "mv" && flag == "-f" => {
                        let content = files
                            .remove(&key(from))
                            .ok_or_else(|| anyhow::anyhow!("mv: {}: No such file", from))?;
                        files.insert(key(to), content);
                    }
                    [cat, path] if cat == "cat" => {
                        if let Some(over) = self.exec_read_override.lock().unwrap().as_ref() {
                            output.push_str(over);
                        } else {
                            let content = files
                                .get(&key(path))
                                .ok_or_else(|| anyhow::anyhow!("cat: {}: No such file", path))?;
                            output.push_str(content);
                        }
                    }
                    other => anyhow::bail!("mock exec: unsupported command {:?}", other),
                }
            }
            Ok(output)
        }
    }
}
