use anyhow::{Context, Result};
use async_trait::async_trait;
use bollard::Docker;
use bollard::models::{ContainerCreateBody, NetworkCreateRequest};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, InspectNetworkOptions,
    ListContainersOptions, LogsOptions, RemoveContainerOptions, StopContainerOptions,
};
use futures::{Stream, StreamExt};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;
use tracing::{debug, instrument};

use super::traits::DockerHostApi;
use crate::config::types::HostConfig;
use crate::ssh::SshSession;
use crate::ssh::tunnel::Tunnel;

/// Default timeout for a Docker API request (until response headers).
const CLIENT_TIMEOUT_SECS: u64 = 120;

/// An image pull fails if Docker reports no progress for this long.
const PULL_STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// An image pull fails if it has not finished after this long.
const PULL_TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Extra time allowed on top of a stop's grace period for Docker to reply.
const STOP_TIMEOUT_MARGIN: Duration = Duration::from_secs(30);

/// Timeout for a command run with [`DockerHost::exec_in_container`].
const EXEC_TIMEOUT: Duration = Duration::from_secs(60);

/// Docker client connected to a remote host via SSH.
pub struct DockerHost {
    client: Docker,
    pub host_name: String,
    /// Keeps the SSH tunnel (and its private socket directory) alive for as
    /// long as the client may use it. `None` for a local Docker connection.
    _tunnel: Option<Tunnel>,
}

impl DockerHost {
    /// Connect to Docker on a remote host via SSH.
    ///
    /// Uses ssh2 to establish an SSH session, then starts a local Unix socket
    /// proxy that bridges connections to the remote Docker socket via
    /// `channel_direct_streamlocal`. bollard connects to the local socket.
    #[instrument(skip_all, fields(host = %host.name))]
    pub async fn connect(host: &HostConfig) -> Result<Self> {
        let ssh = crate::ssh::session::SshSession::connect(host)
            .with_context(|| format!("SSH connection failed to {}", host.name))?;
        Self::connect_with_session(host, &ssh).await
    }

    /// Connect to Docker on a remote host via an existing SSH session.
    pub async fn connect_with_session(host: &HostConfig, ssh: &SshSession) -> Result<Self> {
        let docker_socket = host
            .docker_socket
            .as_deref()
            .unwrap_or("/var/run/docker.sock");

        debug!(
            "Connecting to {}@{}:{} (docker: {})",
            host.user,
            host.ssh_address(),
            host.port,
            docker_socket
        );

        // Bridge a private local Unix socket to the remote Docker socket.
        let tunnel = ssh
            .start_tunnel(docker_socket)
            .with_context(|| format!("Failed to start SSH tunnel to {}", host.name))?;
        let socket_path = tunnel.socket_path().to_str().with_context(|| {
            format!(
                "Tunnel socket path {} is not valid UTF-8",
                tunnel.socket_path().display()
            )
        })?;

        let client = Docker::connect_with_unix(
            socket_path,
            CLIENT_TIMEOUT_SECS,
            bollard::API_DEFAULT_VERSION,
        )
        .with_context(|| format!("Failed to connect bollard on {}", host.name))?;

        // Verify connection
        let _: String = client
            .ping()
            .await
            .with_context(|| format!("Docker ping failed on {}", host.name))?;

        debug!("Docker connected on {}", host.name);
        Ok(Self {
            client,
            host_name: host.name.clone(),
            _tunnel: Some(tunnel),
        })
    }

    /// Connect to a local Docker instance.
    pub fn connect_local() -> Result<Self> {
        let client =
            Docker::connect_with_local_defaults().context("Failed to connect to local Docker")?;
        Ok(Self {
            client,
            host_name: "local".to_string(),
            _tunnel: None,
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
    pub async fn pull_image(
        &self,
        image: &str,
        auth: Option<bollard::auth::DockerCredentials>,
    ) -> Result<()> {
        debug!("Pulling image: {}", image);

        let (repo, tag) = parse_image_ref(image);
        let options = CreateImageOptions {
            from_image: Some(repo.to_string()),
            tag: Some(tag.to_string()),
            ..Default::default()
        };

        let stream = self.client.create_image(Some(options), None, auth);
        drain_with_timeouts(stream, PULL_STALL_TIMEOUT, PULL_TOTAL_TIMEOUT, |_| Ok(()))
            .await
            .with_context(|| format!("Failed to pull {} on {}", image, self.host_name))
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
    ///
    /// Docker only replies once the container has exited, which can take the
    /// whole grace period, so the request timeout is the grace period plus a
    /// margin instead of the client default.
    pub async fn stop_container(&self, id: &str, timeout_secs: i64) -> Result<()> {
        let grace = timeout_secs.clamp(0, i64::from(i32::MAX));
        let options = StopContainerOptions {
            t: Some(grace as i32),
            signal: None,
        };
        self.client
            .clone()
            .with_timeout(stop_request_timeout(grace))
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
        match self
            .client
            .inspect_network(name, None::<InspectNetworkOptions>)
            .await
        {
            Ok(_) => {
                debug!("Network {} already exists on {}", name, self.host_name);
                Ok(())
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                debug!("Creating network {} on {}", name, self.host_name);
                let options = NetworkCreateRequest {
                    name: name.to_string(),
                    driver: Some("bridge".to_string()),
                    ..Default::default()
                };
                self.client.create_network(options).await.with_context(|| {
                    format!("Failed to create network {} on {}", name, self.host_name)
                })?;
                Ok(())
            }
            Err(e) => Err(e).with_context(|| {
                format!("Failed to inspect network {} on {}", name, self.host_name)
            }),
        }
    }

    /// Execute a command inside a running container and return stdout.
    ///
    /// Fails if the output stream breaks, the command exits non-zero, or it
    /// does not finish within [`EXEC_TIMEOUT`].
    pub async fn exec_in_container(&self, container: &str, cmd: &[&str]) -> Result<String> {
        let display = cmd.join(" ");
        tokio::time::timeout(EXEC_TIMEOUT, self.exec_in_container_inner(container, cmd))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Command in {} on {} did not finish within {}s",
                    container,
                    self.host_name,
                    EXEC_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| {
                format!(
                    "Command `{}` failed in {} on {}",
                    truncate_for_error(&display, 120),
                    container,
                    self.host_name
                )
            })
    }

    async fn exec_in_container_inner(&self, container: &str, cmd: &[&str]) -> Result<String> {
        use bollard::container::LogOutput;
        use bollard::exec::CreateExecOptions;

        let exec_config = CreateExecOptions {
            cmd: Some(cmd.iter().map(|s| s.to_string()).collect()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };

        let exec = self
            .client
            .create_exec(container, exec_config)
            .await
            .context("Failed to create exec")?;

        let output = self
            .client
            .start_exec(&exec.id, None)
            .await
            .context("Failed to start exec")?;

        let mut stdout = String::new();
        let mut stderr = String::new();
        if let bollard::exec::StartExecResults::Attached { mut output, .. } = output {
            while let Some(msg) = output.next().await {
                match msg.context("Exec output stream failed")? {
                    LogOutput::StdErr { message } => {
                        stderr.push_str(&String::from_utf8_lossy(&message))
                    }
                    other => stdout.push_str(&other.to_string()),
                }
            }
        }

        // The stream ends when the process closes its output; the exit code
        // may be recorded a moment later.
        let mut delay = Duration::from_millis(10);
        let exit_code = loop {
            let inspect = self
                .client
                .inspect_exec(&exec.id)
                .await
                .context("Failed to inspect exec")?;
            if inspect.running != Some(true) {
                break inspect.exit_code;
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_millis(500));
        };

        check_exec_exit(exit_code, &stderr)?;
        Ok(stdout)
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

    async fn pull_image(
        &self,
        image: &str,
        auth: Option<bollard::auth::DockerCredentials>,
    ) -> Result<()> {
        DockerHost::pull_image(self, image, auth).await
    }

    async fn create_container(&self, name: &str, config: ContainerCreateBody) -> Result<String> {
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

/// Request timeout for stopping a container with a `grace_secs` grace period.
fn stop_request_timeout(grace_secs: i64) -> Duration {
    Duration::from_secs(grace_secs.max(0) as u64) + STOP_TIMEOUT_MARGIN
}

/// Turn an exec's exit code into a result.
fn check_exec_exit(exit_code: Option<i64>, stderr: &str) -> Result<()> {
    match exit_code {
        Some(0) => Ok(()),
        Some(code) => {
            let stderr = stderr.trim();
            if stderr.is_empty() {
                anyhow::bail!("exited with code {}", code)
            } else {
                anyhow::bail!(
                    "exited with code {}: {}",
                    code,
                    truncate_for_error(stderr, 500)
                )
            }
        }
        None => anyhow::bail!("Docker did not report an exit code"),
    }
}

fn truncate_for_error(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// Consume a progress stream, failing if no item arrives within `stall` or the
/// whole stream takes longer than `total`.
///
/// bollard's client timeout only covers the request until response headers;
/// a streamed body (e.g. image pull progress) can otherwise stall forever.
async fn drain_with_timeouts<S, T, E>(
    stream: S,
    stall: Duration,
    total: Duration,
    mut on_item: impl FnMut(T) -> Result<()>,
) -> Result<()>
where
    S: Stream<Item = std::result::Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut stream = std::pin::pin!(stream);
    let deadline = Instant::now() + total;
    loop {
        let wait_until = (Instant::now() + stall).min(deadline);
        match tokio::time::timeout_at(wait_until, stream.next()).await {
            Ok(Some(item)) => on_item(item?)?,
            Ok(None) => return Ok(()),
            Err(_) if Instant::now() >= deadline => {
                anyhow::bail!("did not finish within {}s", total.as_secs())
            }
            Err(_) => anyhow::bail!("no progress for {}s", stall.as_secs()),
        }
    }
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
        assert_eq!(parse_image_ref("myapp:sha-abc123"), ("myapp", "sha-abc123"));
    }

    #[test]
    fn test_stop_request_timeout_covers_grace_period() {
        assert_eq!(stop_request_timeout(10), Duration::from_secs(40));
        assert_eq!(stop_request_timeout(300), Duration::from_secs(330));
        assert_eq!(stop_request_timeout(-5), STOP_TIMEOUT_MARGIN);
        assert!(stop_request_timeout(200) > Duration::from_secs(CLIENT_TIMEOUT_SECS));
    }

    #[test]
    fn test_check_exec_exit() {
        assert!(check_exec_exit(Some(0), "").is_ok());
        let err = check_exec_exit(Some(2), "sh: no such file\n").unwrap_err();
        assert_eq!(err.to_string(), "exited with code 2: sh: no such file");
        let err = check_exec_exit(Some(1), "  ").unwrap_err();
        assert_eq!(err.to_string(), "exited with code 1");
        assert!(check_exec_exit(None, "").is_err());
    }

    #[test]
    fn test_truncate_for_error_respects_char_boundaries() {
        assert_eq!(truncate_for_error("short", 10), "short");
        assert_eq!(truncate_for_error("abcdef", 3), "abc...");
        assert_eq!(truncate_for_error("aé", 2), "a...");
    }

    #[derive(Debug)]
    struct TestError;
    impl std::fmt::Display for TestError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "stream broke")
        }
    }
    impl std::error::Error for TestError {}

    #[tokio::test]
    async fn test_drain_with_timeouts_completes() {
        let stream = futures::stream::iter(vec![Ok::<_, TestError>(1), Ok(2), Ok(3)]);
        let mut seen = Vec::new();
        drain_with_timeouts(
            stream,
            Duration::from_secs(5),
            Duration::from_secs(5),
            |i| {
                seen.push(i);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(seen, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_drain_with_timeouts_fails_on_stall() {
        let stream =
            futures::stream::iter(vec![Ok::<u32, TestError>(1)]).chain(futures::stream::pending());
        let err = drain_with_timeouts(
            stream,
            Duration::from_millis(30),
            Duration::from_secs(5),
            |_| Ok(()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no progress"), "{}", err);
    }

    #[tokio::test]
    async fn test_drain_with_timeouts_enforces_total_deadline() {
        // Items keep arriving (never stalls) but the whole stream is too slow.
        let stream = futures::stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Some((Ok::<u32, TestError>(0), ()))
        });
        let err = drain_with_timeouts(
            stream,
            Duration::from_secs(5),
            Duration::from_millis(60),
            |_| Ok(()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{}", err);
    }

    #[tokio::test]
    async fn test_drain_with_timeouts_propagates_stream_errors() {
        let stream = futures::stream::iter(vec![Ok(1), Err(TestError)]);
        let err = drain_with_timeouts(
            stream,
            Duration::from_secs(5),
            Duration::from_secs(5),
            |_: u32| Ok(()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("stream broke"), "{}", err);
    }

    #[test]
    fn test_parse_image_ref_nested_path() {
        assert_eq!(
            parse_image_ref("ghcr.io/org/team/app:latest"),
            ("ghcr.io/org/team/app", "latest")
        );
    }
}
