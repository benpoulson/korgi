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

        // Authenticate with key file
        if let Some(key_path) = &host.ssh_key {
            let key_path = expand_tilde(key_path);
            let key = russh_keys::load_secret_key(&key_path, None)
                .with_context(|| format!("Failed to load SSH key: {}", key_path))?;
            let authenticated = handle
                .authenticate_publickey(&host.user, Arc::new(key))
                .await
                .with_context(|| format!("SSH auth failed for {}", host.name))?;
            if !authenticated {
                anyhow::bail!("SSH public key authentication rejected by {}", host.name);
            }
        } else {
            anyhow::bail!(
                "No ssh_key configured for host '{}'. SSH agent auth not yet supported.",
                host.name
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
