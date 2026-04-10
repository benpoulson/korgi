use anyhow::{Context, Result};
use serde::Serialize;
use ssh2::{CheckResult, KnownHostFileKind, KnownHostKeyFormat};
use std::io::{IsTerminal, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
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

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SshConnectStatus {
    Pass,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct SshConnectStep {
    pub status: SshConnectStatus,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl SshConnectStep {
    fn pass(message: impl Into<String>) -> Self {
        Self {
            status: SshConnectStatus::Pass,
            message: message.into(),
            hint: None,
        }
    }

    fn fail(message: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            status: SshConnectStatus::Fail,
            message: message.into(),
            hint,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SshConnectReport {
    pub tcp_connect: SshConnectStep,
    pub handshake: SshConnectStep,
    pub host_key: SshConnectStep,
    pub authentication: SshConnectStep,
}

impl SshConnectReport {
    fn blocked(blocked_by: &str) -> SshConnectStep {
        SshConnectStep::fail(format!("Not attempted because {} failed", blocked_by), None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostKeyPromptDecision {
    TrustAndStore,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostKeyDecision {
    Match,
    TrustedNewKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostKeyError {
    MissingKnownHostsEntry { interactive: bool },
    PromptRejected,
    Mismatch,
    CheckFailed,
}

pub struct SshConnectAttempt {
    pub report: SshConnectReport,
    pub session: Option<SshSession>,
    pub error: Option<anyhow::Error>,
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
        let attempt = Self::connect_with_report(host);
        match (attempt.session, attempt.error) {
            (Some(session), _) => Ok(session),
            (_, Some(err)) => Err(err),
            _ => anyhow::bail!("SSH connection failed for {}", host.name),
        }
    }

    #[instrument(skip_all, fields(host = %host.name, address = %host.address))]
    pub fn connect_with_report(host: &HostConfig) -> SshConnectAttempt {
        debug!(
            "Connecting to {} ({}@{}:{})",
            host.name,
            host.user,
            host.ssh_address(),
            host.port
        );

        let mut report = SshConnectReport {
            tcp_connect: SshConnectReport::blocked("tcp_connect"),
            handshake: SshConnectReport::blocked("handshake"),
            host_key: SshConnectReport::blocked("host_key"),
            authentication: SshConnectReport::blocked("authentication"),
        };

        let addr = format!("{}:{}", host.ssh_address(), host.port);
        let tcp = match TcpStream::connect(&addr) {
            Ok(tcp) => {
                report.tcp_connect = SshConnectStep::pass(format!("Connected to {}", addr));
                tcp
            }
            Err(e) => {
                report.tcp_connect = SshConnectStep::fail(
                    format!("TCP connection failed to {}", addr),
                    Some(e.to_string()),
                );
                return SshConnectAttempt {
                    report,
                    session: None,
                    error: Some(anyhow::anyhow!("TCP connection failed to {}: {}", addr, e)),
                };
            }
        };

        let mut session = match ssh2::Session::new().with_context(|| "Failed to create SSH session")
        {
            Ok(session) => session,
            Err(e) => {
                report.handshake =
                    SshConnectStep::fail("Failed to create SSH session", Some(e.to_string()));
                return SshConnectAttempt {
                    report,
                    session: None,
                    error: Some(e),
                };
            }
        };
        session.set_tcp_stream(tcp);
        if let Err(e) = session
            .handshake()
            .with_context(|| format!("SSH handshake failed with {}", host.name))
        {
            report.handshake =
                SshConnectStep::fail(format!("SSH handshake failed with {}", host.name), None);
            return SshConnectAttempt {
                report,
                session: None,
                error: Some(e),
            };
        }
        report.handshake = SshConnectStep::pass(format!("SSH handshake completed with {}", addr));

        match verify_host_key(&session, host) {
            Ok(HostKeyDecision::Match) => {
                report.host_key = SshConnectStep::pass(format!(
                    "Host key matched {}",
                    known_hosts_path_display()
                ));
            }
            Ok(HostKeyDecision::TrustedNewKey) => {
                report.host_key = SshConnectStep::pass(format!(
                    "Trusted host key and added it to {}",
                    known_hosts_path_display()
                ));
            }
            Err(HostKeyError::MissingKnownHostsEntry { interactive: false }) => {
                report.host_key = SshConnectStep::fail(
                    "Host key is missing from ~/.ssh/known_hosts",
                    Some(format!(
                        "Add {} to known_hosts or rerun from an interactive terminal",
                        host_display_for_known_hosts(host)
                    )),
                );
                report.authentication = SshConnectReport::blocked("host key verification");
                return SshConnectAttempt {
                    report,
                    session: None,
                    error: Some(anyhow::anyhow!(
                        "Host key for {} is missing from ~/.ssh/known_hosts. Add it first or rerun interactively.",
                        host_display_for_known_hosts(host)
                    )),
                };
            }
            Err(HostKeyError::MissingKnownHostsEntry { interactive: true }) => {
                report.host_key = SshConnectStep::fail(
                    "Host key is missing from ~/.ssh/known_hosts",
                    Some("Interactive trust flow did not complete".to_string()),
                );
                report.authentication = SshConnectReport::blocked("host key verification");
                return SshConnectAttempt {
                    report,
                    session: None,
                    error: Some(anyhow::anyhow!(
                        "Host key for {} is missing from ~/.ssh/known_hosts",
                        host_display_for_known_hosts(host)
                    )),
                };
            }
            Err(HostKeyError::PromptRejected) => {
                report.host_key = SshConnectStep::fail(
                    "Host key trust was declined",
                    Some("Connection aborted because the host key is not trusted".to_string()),
                );
                report.authentication = SshConnectReport::blocked("host key verification");
                return SshConnectAttempt {
                    report,
                    session: None,
                    error: Some(anyhow::anyhow!(
                        "Host key trust declined for {}",
                        host_display_for_known_hosts(host)
                    )),
                };
            }
            Err(HostKeyError::Mismatch) => {
                report.host_key = SshConnectStep::fail(
                    "Host key mismatch",
                    Some(format!(
                        "Refusing to connect. Check {} for a stale or compromised entry.",
                        known_hosts_path_display()
                    )),
                );
                report.authentication = SshConnectReport::blocked("host key verification");
                return SshConnectAttempt {
                    report,
                    session: None,
                    error: Some(anyhow::anyhow!(
                        "Host key mismatch for {}. Refusing to connect.",
                        host_display_for_known_hosts(host)
                    )),
                };
            }
            Err(HostKeyError::CheckFailed) => {
                report.host_key = SshConnectStep::fail(
                    "Known hosts verification failed",
                    Some(format!(
                        "Could not verify {} against {}",
                        host_display_for_known_hosts(host),
                        known_hosts_path_display()
                    )),
                );
                report.authentication = SshConnectReport::blocked("host key verification");
                return SshConnectAttempt {
                    report,
                    session: None,
                    error: Some(anyhow::anyhow!(
                        "Known hosts verification failed for {}",
                        host_display_for_known_hosts(host)
                    )),
                };
            }
        }

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
                            match prompt_passphrase(key_path, attempt, MAX_PASSPHRASE_ATTEMPTS) {
                                Ok(passphrase) => passphrase,
                                Err(e) => {
                                    report.authentication = SshConnectStep::fail(
                                        "Failed to read SSH key passphrase",
                                        Some(e.to_string()),
                                    );
                                    return SshConnectAttempt {
                                        report,
                                        session: None,
                                        error: Some(e),
                                    };
                                }
                            };
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
            let password = match prompt_password(&host.user, host.ssh_address()) {
                Ok(password) => password,
                Err(e) => {
                    report.authentication =
                        SshConnectStep::fail("Failed to read SSH password", Some(e.to_string()));
                    return SshConnectAttempt {
                        report,
                        session: None,
                        error: Some(e),
                    };
                }
            };
            if let Err(e) = session
                .userauth_password(&host.user, &password)
                .with_context(|| format!("Password auth failed for {}", host.name))
            {
                report.authentication = SshConnectStep::fail(
                    format!("Password authentication failed for {}", host.name),
                    Some(e.to_string()),
                );
                return SshConnectAttempt {
                    report,
                    session: None,
                    error: Some(e),
                };
            }
            authenticated = session.authenticated();
        }

        if !authenticated {
            report.authentication = SshConnectStep::fail(
                format!("SSH authentication failed for {}@{}", host.user, host.name),
                None,
            );
            return SshConnectAttempt {
                report,
                session: None,
                error: Some(anyhow::anyhow!(
                    "SSH authentication failed for {}@{}",
                    host.user,
                    host.name
                )),
            };
        }
        report.authentication =
            SshConnectStep::pass(format!("Authenticated as {} using SSH", host.user));

        debug!("Connected to {}", host.name);
        SshConnectAttempt {
            report,
            session: Some(Self {
                session,
                host: host.clone(),
            }),
            error: None,
        }
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

fn verify_host_key(
    session: &ssh2::Session,
    host: &HostConfig,
) -> std::result::Result<HostKeyDecision, HostKeyError> {
    let (key, key_type) = session.host_key().ok_or(HostKeyError::CheckFailed)?;
    let known_hosts_path = known_hosts_path().map_err(|_| HostKeyError::CheckFailed)?;
    let mut known_hosts = session
        .known_hosts()
        .map_err(|_| HostKeyError::CheckFailed)?;

    if known_hosts_path.exists() {
        known_hosts
            .read_file(&known_hosts_path, KnownHostFileKind::OpenSSH)
            .map_err(|_| HostKeyError::CheckFailed)?;
    }

    match known_hosts_result(&known_hosts, host.ssh_address(), host.port, key) {
        CheckResult::Match => Ok(HostKeyDecision::Match),
        CheckResult::NotFound => {
            if !is_interactive_terminal() {
                return Err(HostKeyError::MissingKnownHostsEntry { interactive: false });
            }

            let fingerprint = format_fingerprint(session);
            match prompt_host_key_trust(host, &fingerprint) {
                Ok(HostKeyPromptDecision::TrustAndStore) => {
                    let host_entry = known_hosts_entry_name(host.ssh_address(), host.port);
                    let comment = format!("korgi {}", host.name);
                    known_hosts
                        .add(
                            &host_entry,
                            key,
                            &comment,
                            KnownHostKeyFormat::from(key_type),
                        )
                        .map_err(|_| HostKeyError::CheckFailed)?;
                    persist_known_hosts(&known_hosts, &known_hosts_path)
                        .map_err(|_| HostKeyError::CheckFailed)?;
                    Ok(HostKeyDecision::TrustedNewKey)
                }
                Ok(HostKeyPromptDecision::Reject) => Err(HostKeyError::PromptRejected),
                Err(_) => Err(HostKeyError::CheckFailed),
            }
        }
        CheckResult::Mismatch => Err(HostKeyError::Mismatch),
        CheckResult::Failure => Err(HostKeyError::CheckFailed),
    }
}

fn known_hosts_result(
    known_hosts: &ssh2::KnownHosts,
    ssh_address: &str,
    port: u16,
    key: &[u8],
) -> CheckResult {
    if port == 22 {
        known_hosts.check(ssh_address, key)
    } else {
        known_hosts.check_port(ssh_address, port, key)
    }
}

fn persist_known_hosts(known_hosts: &ssh2::KnownHosts, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    known_hosts
        .write_file(path, KnownHostFileKind::OpenSSH)
        .with_context(|| format!("Failed to update {}", path.display()))?;
    Ok(())
}

fn known_hosts_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").with_context(|| "HOME is not set")?;
    Ok(PathBuf::from(home).join(".ssh").join("known_hosts"))
}

fn known_hosts_path_display() -> String {
    known_hosts_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "~/.ssh/known_hosts".to_string())
}

fn known_hosts_entry_name(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{}]:{}", host, port)
    }
}

fn host_display_for_known_hosts(host: &HostConfig) -> String {
    known_hosts_entry_name(host.ssh_address(), host.port)
}

fn is_interactive_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

fn prompt_host_key_trust(host: &HostConfig, fingerprint: &str) -> Result<HostKeyPromptDecision> {
    eprintln!(
        "Unknown SSH host key for {} ({})",
        host.name,
        host_display_for_known_hosts(host)
    );
    eprintln!("Fingerprint: {}", fingerprint);
    eprint!("Trust this host key and add it to ~/.ssh/known_hosts? [y/N] ");
    std::io::stderr().flush().ok();

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .with_context(|| "Failed to read host key trust prompt")?;

    if matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
        Ok(HostKeyPromptDecision::TrustAndStore)
    } else {
        Ok(HostKeyPromptDecision::Reject)
    }
}

fn format_fingerprint(session: &ssh2::Session) -> String {
    session
        .host_key_hash(ssh2::HashType::Sha256)
        .map(|bytes| {
            bytes
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<Vec<_>>()
                .join(":")
        })
        .unwrap_or_else(|| "unavailable".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_hosts_entry_name_default_port() {
        assert_eq!(known_hosts_entry_name("example.com", 22), "example.com");
    }

    #[test]
    fn test_known_hosts_entry_name_non_default_port() {
        assert_eq!(
            known_hosts_entry_name("example.com", 2222),
            "[example.com]:2222"
        );
    }

    #[test]
    fn test_known_hosts_result_uses_port_aware_lookup() {
        let dir = tempfile::TempDir::new().unwrap();
        let session = ssh2::Session::new().unwrap();
        let mut known_hosts = session.known_hosts().unwrap();
        let key = [1u8, 2, 3, 4];
        known_hosts
            .add(
                "[example.com]:2222",
                &key,
                "test",
                KnownHostKeyFormat::SshRsa,
            )
            .unwrap();
        assert!(matches!(
            known_hosts_result(&known_hosts, "example.com", 2222, &key),
            CheckResult::Match
        ));
        let path = dir.path().join("known_hosts");
        persist_known_hosts(&known_hosts, &path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_host_key_error_variants() {
        assert_eq!(HostKeyError::Mismatch, HostKeyError::Mismatch);
        assert_eq!(HostKeyError::PromptRejected, HostKeyError::PromptRejected);
        assert_eq!(
            HostKeyError::MissingKnownHostsEntry { interactive: false },
            HostKeyError::MissingKnownHostsEntry { interactive: false }
        );
    }
}
