use anyhow::{Context, Result};
use serde::Serialize;
use ssh2::{CheckResult, KnownHostFileKind, KnownHostKeyFormat};
use std::io::{IsTerminal, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, instrument};

use super::nonblocking::{read_both_until_eof, retry_eagain};
use crate::config::types::HostConfig;

const MAX_PASSPHRASE_ATTEMPTS: usize = 3;

/// Default TCP connect timeout. Override with `KORGI_SSH_CONNECT_TIMEOUT`
/// (whole seconds).
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Environment variable overriding the TCP connect timeout (seconds).
const CONNECT_TIMEOUT_ENV: &str = "KORGI_SSH_CONNECT_TIMEOUT";

/// Timeout for each blocking libssh2 operation during connection setup
/// (handshake, host key and authentication exchanges).
const SESSION_SETUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval between SSH keepalive messages.
pub const KEEPALIVE_INTERVAL_SECS: u32 = 15;

/// Default timeout for commands run with [`SshSession::exec`].
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for opening a channel to a remote Unix socket.
const CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(30);

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
///
/// Once connected the session is in non-blocking mode: every operation on it
/// must tolerate `LIBSSH2_ERROR_EAGAIN` (see [`super::nonblocking`]). This lets
/// the Docker tunnel multiplex channels without one stalled channel holding
/// the session lock for everyone else.
pub struct SshSession {
    session: ssh2::Session,
    /// Serialises channel opens: libssh2 keeps channel-open state per session.
    open_lock: Arc<Mutex<()>>,
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
        let connect_timeout = connect_timeout_from_env(std::env::var(CONNECT_TIMEOUT_ENV).ok());
        let tcp = match connect_tcp(&addr, connect_timeout) {
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
        // Bound every blocking libssh2 call made while setting up the session
        // so an unresponsive server cannot hang the handshake or auth forever.
        session.set_timeout(duration_to_ms(SESSION_SETUP_TIMEOUT));
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
        let mut skipped_passphrase_keys: Vec<String> = Vec::new();

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
                Err(_) if !is_interactive_terminal() => {
                    // The key may be encrypted, but nobody can type a passphrase:
                    // never block on a prompt. Fall through to ssh-agent.
                    debug!(
                        "Key {} was not accepted without a passphrase; not prompting (no interactive terminal)",
                        key_path
                    );
                    skipped_passphrase_keys.push(key_path.clone());
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
            let password =
                match prompt_password(&host.user, host.ssh_address(), &skipped_passphrase_keys) {
                    Ok(password) => password,
                    Err(e) => {
                        report.authentication = SshConnectStep::fail(
                            "Failed to read SSH password",
                            Some(e.to_string()),
                        );
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

        // Detect and prevent idle drops (NAT, firewalls). Keepalives are sent
        // by whoever drives the session (the Docker tunnel pump).
        session.set_keepalive(true, KEEPALIVE_INTERVAL_SECS);
        // From here on nothing may block on the session; see `SshSession` docs.
        session.set_blocking(false);

        debug!("Connected to {}", host.name);
        SshConnectAttempt {
            report,
            session: Some(Self {
                session,
                open_lock: Arc::new(Mutex::new(())),
                host: host.clone(),
            }),
            error: None,
        }
    }

    /// Execute a command on the remote host with the default timeout.
    pub fn exec(&self, command: &str) -> Result<ExecOutput> {
        self.exec_with_timeout(command, DEFAULT_EXEC_TIMEOUT)
    }

    /// Execute a command on the remote host, failing after `timeout`.
    ///
    /// stdout and stderr are read together so a chatty stderr cannot fill the
    /// channel window and deadlock the read of stdout.
    pub fn exec_with_timeout(&self, command: &str, timeout: Duration) -> Result<ExecOutput> {
        let deadline = Instant::now() + timeout;
        let host = &self.host.name;

        let mut channel = {
            let _guard = self.open_lock.lock().unwrap_or_else(|p| p.into_inner());
            retry_eagain(deadline, "open a session channel", || {
                self.session.channel_session()
            })
            .with_context(|| format!("Failed to open channel on {}", host))?
        };

        retry_eagain(deadline, "start the command", || channel.exec(command))
            .with_context(|| format!("Failed to exec on {}", host))?;

        let mut stdout_stream = channel.stream(0);
        let mut stderr_stream = channel.stderr();
        let (stdout, stderr) = read_both_until_eof(
            &mut stdout_stream,
            &mut stderr_stream,
            || channel.eof(),
            deadline,
        )
        .with_context(|| format!("Failed to read output of `{}` on {}", command, host))?;

        retry_eagain(deadline, "close the channel", || channel.wait_close())
            .with_context(|| format!("Failed to close channel on {}", host))?;
        let exit_code = channel
            .exit_status()
            .with_context(|| format!("Failed to read exit status on {}", host))?;

        Ok(ExecOutput {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code: u32::try_from(exit_code).ok(),
        })
    }

    /// Open a direct-streamlocal channel to a Unix socket on the remote host.
    ///
    /// The returned channel is in non-blocking mode.
    pub fn channel_direct_streamlocal(&self, socket_path: &str) -> Result<ssh2::Channel> {
        super::tunnel::open_streamlocal(
            &self.session,
            &self.open_lock,
            socket_path,
            CHANNEL_OPEN_TIMEOUT,
        )
        .with_context(|| {
            format!(
                "Failed to open channel to {} on {}",
                socket_path, self.host.name
            )
        })
    }

    /// Start a local Unix socket tunnel to `remote_socket` on this host.
    pub fn start_tunnel(&self, remote_socket: &str) -> Result<super::tunnel::Tunnel> {
        super::tunnel::Tunnel::start(
            self.session.clone(),
            self.open_lock.clone(),
            remote_socket,
            &self.host.name,
        )
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

/// Fail instead of prompting when nobody can answer the prompt.
///
/// Reading a passphrase or password without a terminal would block forever
/// (CI, cron, piped input).
fn ensure_can_prompt(interactive: bool, what: &str) -> Result<()> {
    if interactive {
        Ok(())
    } else {
        anyhow::bail!(
            "{} is required but no interactive terminal is available; load your key into ssh-agent or run korgi from a terminal",
            what
        )
    }
}

/// Prompt the user for an SSH key passphrase.
fn prompt_passphrase(key_path: &str, attempt: usize, max_attempts: usize) -> Result<String> {
    ensure_can_prompt(
        is_interactive_terminal(),
        &format!("A passphrase for {}", key_path),
    )?;
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
///
/// `skipped_keys` lists keys that may be encrypted but could not be unlocked
/// because no terminal was available; they are named in the error.
fn prompt_password(user: &str, host: &str, skipped_keys: &[String]) -> Result<String> {
    let what = if skipped_keys.is_empty() {
        format!(
            "SSH password authentication for {}@{} (no key or agent identity was accepted)",
            user, host
        )
    } else {
        format!(
            "SSH authentication for {}@{} (key {} may need a passphrase)",
            user,
            host,
            skipped_keys.join(", ")
        )
    };
    ensure_can_prompt(is_interactive_terminal(), &what)?;
    eprint!("{}@{}'s password: ", user, host);
    let password = rpassword::read_password().with_context(|| "Failed to read password")?;
    Ok(password)
}

/// Parse the connect timeout override; invalid or zero values use the default.
fn connect_timeout_from_env(value: Option<String>) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT)
}

fn duration_to_ms(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// Connect to `addr` ("host:port"), trying each resolved address with
/// `timeout` so an unreachable host fails quickly instead of hanging for the
/// OS default (minutes).
fn connect_tcp(addr: &str, timeout: Duration) -> std::io::Result<TcpStream> {
    let addrs: Vec<SocketAddr> = addr.to_socket_addrs()?.collect();
    connect_any(&addrs, timeout)
}

fn connect_any(addrs: &[SocketAddr], timeout: Duration) -> std::io::Result<TcpStream> {
    let mut last_err = None;
    for sock_addr in addrs {
        match TcpStream::connect_timeout(sock_addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                debug!("TCP connect to {} failed: {}", sock_addr, e);
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "address resolved to no socket addresses",
        )
    }))
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
    fn test_connect_timeout_from_env() {
        assert_eq!(connect_timeout_from_env(None), DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(
            connect_timeout_from_env(Some("3".to_string())),
            Duration::from_secs(3)
        );
        assert_eq!(
            connect_timeout_from_env(Some(" 25 ".to_string())),
            Duration::from_secs(25)
        );
        assert_eq!(
            connect_timeout_from_env(Some("0".to_string())),
            DEFAULT_CONNECT_TIMEOUT
        );
        assert_eq!(
            connect_timeout_from_env(Some("soon".to_string())),
            DEFAULT_CONNECT_TIMEOUT
        );
    }

    #[test]
    fn test_duration_to_ms_saturates() {
        assert_eq!(duration_to_ms(Duration::from_secs(30)), 30_000);
        assert_eq!(duration_to_ms(Duration::from_secs(u64::MAX)), u32::MAX);
    }

    #[test]
    fn test_connect_any_succeeds_on_listening_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(connect_any(&[addr], Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn test_connect_any_fails_on_closed_port_and_empty_list() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert!(connect_any(&[addr], Duration::from_secs(2)).is_err());
        assert!(connect_any(&[], Duration::from_secs(2)).is_err());
    }

    #[test]
    fn test_ensure_can_prompt_fails_fast_without_terminal() {
        assert!(ensure_can_prompt(true, "A passphrase").is_ok());
        let err = ensure_can_prompt(false, "A passphrase for ~/.ssh/id_ed25519")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no interactive terminal"), "{}", err);
        assert!(err.contains("id_ed25519"), "{}", err);
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
