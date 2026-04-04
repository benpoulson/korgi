use anyhow::{Context, Result};
use std::collections::HashMap;
use tracing::{debug, info};

use super::session::SshSession;
use crate::config::types::{Config, HostConfig};

/// Pool of SSH connections, one per configured host.
pub struct SshPool {
    sessions: HashMap<String, SshSession>,
}

impl SshPool {
    /// Connect to all configured hosts in parallel.
    pub async fn connect_all(config: &Config) -> Result<Self> {
        info!("Connecting to {} hosts...", config.hosts.len());

        let mut handles = Vec::new();
        for host in &config.hosts {
            let host = host.clone();
            handles.push(tokio::spawn(async move {
                let session = SshSession::connect(&host).await?;
                Ok::<_, anyhow::Error>((host.name.clone(), session))
            }));
        }

        let mut sessions = HashMap::new();
        for handle in handles {
            let (name, session) = handle
                .await
                .context("SSH connection task panicked")?
                .context("SSH connection failed")?;
            sessions.insert(name, session);
        }

        debug!("Connected to all {} hosts", sessions.len());
        Ok(Self { sessions })
    }

    /// Connect to a specific set of hosts by name.
    pub async fn connect_hosts(hosts: &[&HostConfig]) -> Result<Self> {
        let mut handles = Vec::new();
        for host in hosts {
            let host = (*host).clone();
            handles.push(tokio::spawn(async move {
                let session = SshSession::connect(&host).await?;
                Ok::<_, anyhow::Error>((host.name.clone(), session))
            }));
        }

        let mut sessions = HashMap::new();
        for handle in handles {
            let (name, session) = handle
                .await
                .context("SSH connection task panicked")?
                .context("SSH connection failed")?;
            sessions.insert(name, session);
        }

        Ok(Self { sessions })
    }

    /// Get a session by host name.
    pub fn get(&self, host_name: &str) -> Option<&SshSession> {
        self.sessions.get(host_name)
    }

    /// Iterate over all sessions.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &SshSession)> {
        self.sessions.iter()
    }

    /// Get number of connected hosts.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Close all connections.
    pub async fn close(self) {
        for (_, session) in self.sessions {
            session.close().await.ok();
        }
    }
}
