use anyhow::{Context, Result};
use std::collections::HashMap;
use tracing::{debug, info};

use super::session::SshSession;
use crate::config::types::Config;

/// Pool of SSH connections, one per configured host.
pub struct SshPool {
    sessions: HashMap<String, SshSession>,
}

impl SshPool {
    /// Connect to all configured hosts.
    pub fn connect_all(config: &Config) -> Result<Self> {
        info!("Connecting to {} hosts...", config.hosts.len());

        let mut sessions = HashMap::new();
        for host in &config.hosts {
            let session = SshSession::connect(host)
                .with_context(|| format!("Failed to connect to {}", host.name))?;
            sessions.insert(host.name.clone(), session);
        }

        debug!("Connected to all {} hosts", sessions.len());
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
}
