use anyhow::{Context, Result};
use std::io::Read;
use std::net::TcpStream;
use std::path::Path;
use tracing::{debug, instrument};

use crate::config::types::HostConfig;

const MAX_PASSPHRASE_ATTEMPTS: usize = 3;

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

/// Wrapper around an ssh2 SSH session to a single host.
pub struct SshSession {
    session: ssh2::Session,
    pub host: HostConfig,
}

impl SshSession {
    /// Connect to a host via SSH.
    #[instrument(skip_all, fields(host = %host.name, address = %host.address))]
    pub fn connect(host: &HostConfig) -> Result<Self> {
        debug!(
            "Connecting to {} ({}@{}:{})",
            host.name,
            host.user,
            host.ssh_address(),
            host.port
        );

        let addr = format!("{}:{}", host.ssh_address(), host.port);
        let tcp = TcpStream::connect(&addr)
            .with_context(|| format!("TCP connection failed to {}", addr))?;

        let mut session = ssh2::Session::new().with_context(|| "Failed to create SSH session")?;
        session.set_tcp_stream(tcp);
        session
            .handshake()
            .with_context(|| format!("SSH handshake failed with {}", host.name))?;

        // Resolve key paths: explicit config, then defaults
        let key_paths: Vec<String> = if let Some(key_path) = &host.ssh_key {
            vec![expand_tilde(key_path)]
        } else {
            default_key_paths()
        };

        let mut authenticated = false;

        // Try key files
        for key_path in &key_paths {
            if !Path::new(key_path).exists() {
                continue;
            }

            debug!("Trying key: {}", key_path);

            // Try without passphrase first
            match session.userauth_pubkey_file(&host.user, None, Path::new(key_path), None) {
                Ok(()) => {
                    debug!("Authenticated with key {}", key_path);
                    authenticated = true;
                    break;
                }
                Err(_) => {
                    // The key may be encrypted. Allow a few retries before falling back
                    // to other auth methods so a passphrase typo does not immediately
                    // push the user into password authentication.
                    let mut key_authenticated = false;
                    for attempt in 1..=MAX_PASSPHRASE_ATTEMPTS {
                        let passphrase =
                            prompt_passphrase(key_path, attempt, MAX_PASSPHRASE_ATTEMPTS)?;
                        match session.userauth_pubkey_file(
                            &host.user,
                            None,
                            Path::new(key_path),
                            Some(&passphrase),
                        ) {
                            Ok(()) => {
                                debug!("Authenticated with key {} (passphrase)", key_path);
                                authenticated = true;
                                key_authenticated = true;
                                break;
                            }
                            Err(e) => {
                                debug!(
                                    "Key {} failed on passphrase attempt {}: {}",
                                    key_path, attempt, e
                                );
                                if attempt < MAX_PASSPHRASE_ATTEMPTS {
                                    eprintln!("Passphrase rejected. Try again.");
                                }
                            }
                        }
                    }

                    if key_authenticated {
                        break;
                    }
                }
            }
        }

        // Try ssh-agent
        if !authenticated && session.userauth_agent(&host.user).is_ok() {
            debug!("Authenticated via ssh-agent");
            authenticated = true;
        }

        // Fall back to password
        if !authenticated {
            let password = prompt_password(&host.user, host.ssh_address())?;
            session
                .userauth_password(&host.user, &password)
                .with_context(|| format!("Password auth failed for {}", host.name))?;
            authenticated = session.authenticated();
        }

        if !authenticated {
            anyhow::bail!("SSH authentication failed for {}@{}", host.user, host.name);
        }

        debug!("Connected to {}", host.name);
        Ok(Self {
            session,
            host: host.clone(),
        })
    }

    /// Execute a command on the remote host.
    pub fn exec(&self, command: &str) -> Result<ExecOutput> {
        let mut channel = self
            .session
            .channel_session()
            .with_context(|| format!("Failed to open channel on {}", self.host.name))?;

        channel
            .exec(command)
            .with_context(|| format!("Failed to exec on {}", self.host.name))?;

        let mut stdout = String::new();
        channel.read_to_string(&mut stdout).ok();

        let mut stderr = String::new();
        channel.stderr().read_to_string(&mut stderr).ok();

        channel.wait_close().ok();
        let exit_code = channel.exit_status().ok().map(|c| c as u32);

        Ok(ExecOutput {
            stdout,
            stderr,
            exit_code,
        })
    }

    /// Open a direct-streamlocal channel to a Unix socket on the remote host.
    pub fn channel_direct_streamlocal(&self, socket_path: &str) -> Result<ssh2::Channel> {
        self.session
            .channel_direct_streamlocal(socket_path, None)
            .with_context(|| {
                format!(
                    "Failed to open channel to {} on {}",
                    socket_path, self.host.name
                )
            })
    }

    /// Check if the connection is still alive.
    pub fn ping(&self) -> Result<()> {
        let output = self.exec("echo ok")?;
        if output.success() {
            Ok(())
        } else {
            anyhow::bail!("SSH ping failed on {}", self.host.name)
        }
    }

    /// Get the underlying ssh2 session reference.
    pub fn session(&self) -> &ssh2::Session {
        &self.session
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

/// Prompt the user for an SSH key passphrase.
fn prompt_passphrase(key_path: &str, attempt: usize, max_attempts: usize) -> Result<String> {
    if max_attempts > 1 {
        eprint!(
            "Enter passphrase for {} (attempt {}/{}): ",
            key_path, attempt, max_attempts
        );
    } else {
        eprint!("Enter passphrase for {}: ", key_path);
    }
    let passphrase = rpassword::read_password().with_context(|| "Failed to read passphrase")?;
    Ok(passphrase)
}

/// Prompt the user for an SSH password.
fn prompt_password(user: &str, host: &str) -> Result<String> {
    eprint!("{}@{}'s password: ", user, host);
    let password = rpassword::read_password().with_context(|| "Failed to read password")?;
    Ok(password)
}
