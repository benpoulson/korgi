use anyhow::{Context, Result};
use async_trait::async_trait;
use russh::client;
use ssh_key::PublicKey;
use std::sync::Arc;
use tracing::{debug, instrument};

use crate::config::types::HostConfig;

/// Output from an SSH command execution.
#[derive(Debug)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<u32>,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Wrapper around a russh SSH connection to a single host.
pub struct SshSession {
    handle: client::Handle<SshHandler>,
    pub host: HostConfig,
}

struct SshHandler;

#[async_trait]
impl client::Handler for SshHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        // TODO: implement known_hosts checking for production use
        Ok(true)
    }
}

impl SshSession {
    /// Connect to a host via SSH.
    #[instrument(skip_all, fields(host = %host.name, address = %host.address))]
    pub async fn connect(host: &HostConfig) -> Result<Self> {
        debug!(
            "Connecting to {} ({}@{}:{})",
            host.name,
            host.user,
            host.ssh_address(),
            host.port
        );

        let config = Arc::new(client::Config::default());
        let handler = SshHandler;

        let addr = format!("{}:{}", host.ssh_address(), host.port);

        let mut handle = client::connect(config, &addr, handler)
            .await
            .with_context(|| format!("Failed to connect to {}", host.name))?;

        // Resolve key paths to try: explicit config, then defaults
        let key_paths: Vec<String> = if let Some(key_path) = &host.ssh_key {
            vec![expand_tilde(key_path)]
        } else {
            default_key_paths()
        };

        let mut authenticated = false;

        for key_path in &key_paths {
            if !std::path::Path::new(key_path).exists() {
                continue;
            }

            // Try without passphrase first
            let key = match russh_keys::load_secret_key(key_path, None) {
                Ok(key) => key,
                Err(_) => {
                    // Key is encrypted -- prompt for passphrase
                    let passphrase = prompt_passphrase(key_path)?;
                    russh_keys::load_secret_key(key_path, Some(&passphrase))
                        .with_context(|| format!("Failed to decrypt key: {}", key_path))?
                }
            };

            match handle
                .authenticate_publickey(&host.user, Arc::new(key))
                .await
            {
                Ok(true) => {
                    debug!("Authenticated with key {}", key_path);
                    authenticated = true;
                    break;
                }
                Ok(false) => {
                    debug!("Key {} rejected by server", key_path);
                }
                Err(e) => {
                    debug!("Auth error with key {}: {}", key_path, e);
                }
            }
        }

        if !authenticated {
            let tried = if key_paths.is_empty() {
                "no keys found".to_string()
            } else {
                key_paths.join(", ")
            };
            anyhow::bail!(
                "SSH authentication failed for {}@{} (tried: {})",
                host.user,
                host.name,
                tried
            );
        }

        debug!("Connected to {}", host.name);

        Ok(Self {
            handle,
            host: host.clone(),
        })
    }

    /// Execute a command on the remote host.
    #[instrument(skip(self), fields(host = %self.host.name))]
    pub async fn exec(&self, command: &str) -> Result<ExecOutput> {
        debug!("Executing: {}", command);

        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .context("Failed to open SSH channel")?;

        channel
            .exec(true, command)
            .await
            .context("Failed to exec command")?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;

        loop {
            let Some(msg) = channel.wait().await else {
                break;
            };
            match msg {
                russh::ChannelMsg::Data { data } => {
                    stdout.extend_from_slice(&data);
                }
                russh::ChannelMsg::ExtendedData { data, ext: 1 } => {
                    stderr.extend_from_slice(&data);
                }
                russh::ChannelMsg::ExitStatus { exit_status } => {
                    exit_code = Some(exit_status);
                }
                _ => {}
            }
        }

        channel.eof().await.ok();

        Ok(ExecOutput {
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
            exit_code,
        })
    }

    /// Check if the connection is still alive.
    pub async fn ping(&self) -> Result<()> {
        let output = self.exec("echo ok").await?;
        if output.success() {
            Ok(())
        } else {
            anyhow::bail!("SSH ping failed on {}", self.host.name)
        }
    }

    /// Open a direct-streamlocal channel to a Unix socket on the remote host.
    /// Used to tunnel to the Docker daemon socket.
    pub async fn open_direct_streamlocal(
        &self,
        remote_socket: &str,
    ) -> Result<russh::Channel<russh::client::Msg>> {
        let channel = self
            .handle
            .channel_open_direct_streamlocal(remote_socket)
            .await
            .with_context(|| {
                format!(
                    "Failed to open channel to {} on {}",
                    remote_socket, self.host.name
                )
            })?;
        Ok(channel)
    }

    /// Close the SSH connection.
    pub async fn close(self) -> Result<()> {
        self.handle
            .disconnect(russh::Disconnect::ByApplication, "closing", "")
            .await
            .ok();
        Ok(())
    }
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

/// Default SSH key paths to try when no explicit key is configured.
fn default_key_paths() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return vec![];
    }
    vec![
        format!("{}/.ssh/id_ed25519", home),
        format!("{}/.ssh/id_rsa", home),
        format!("{}/.ssh/id_ecdsa", home),
    ]
}

/// Prompt the user for an SSH key passphrase on stderr.
fn prompt_passphrase(key_path: &str) -> Result<String> {
    eprint!("Enter passphrase for {}: ", key_path);
    let passphrase = rpassword::read_password().with_context(|| "Failed to read passphrase")?;
    Ok(passphrase)
}
