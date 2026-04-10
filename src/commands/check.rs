use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;

use crate::cli::output;
use crate::config::types::Config;
use crate::docker::DockerHost;
use crate::ssh::SshSession;
use crate::ssh::session::SshConnectStatus;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticCheck {
    pub id: String,
    pub status: DiagnosticStatus,
    pub scope: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticSummary {
    pub pass: usize,
    pub warn: usize,
    pub fail: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticReport {
    pub summary: DiagnosticSummary,
    pub checks: Vec<DiagnosticCheck>,
}

impl DiagnosticReport {
    fn new() -> Self {
        Self {
            summary: DiagnosticSummary {
                pass: 0,
                warn: 0,
                fail: 0,
            },
            checks: Vec::new(),
        }
    }

    fn add(
        &mut self,
        id: impl Into<String>,
        scope: impl Into<String>,
        status: DiagnosticStatus,
        message: impl Into<String>,
        hint: Option<String>,
    ) {
        match status {
            DiagnosticStatus::Pass => self.summary.pass += 1,
            DiagnosticStatus::Warn => self.summary.warn += 1,
            DiagnosticStatus::Fail => self.summary.fail += 1,
        }
        self.checks.push(DiagnosticCheck {
            id: id.into(),
            status,
            scope: scope.into(),
            message: message.into(),
            hint,
        });
    }

    fn has_failures(&self) -> bool {
        self.summary.fail > 0
    }
}

pub async fn run(config: &Config, json_output: bool) -> Result<()> {
    let mut report = DiagnosticReport::new();

    collect_config_diagnostics(config, &mut report);
    collect_ssh_and_docker_diagnostics(config, &mut report).await;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        render_report(&report);
    }

    if report.has_failures() {
        anyhow::bail!("korgi check found {} failing checks", report.summary.fail);
    }

    Ok(())
}

fn collect_config_diagnostics(config: &Config, report: &mut DiagnosticReport) {
    report.add(
        "config.load",
        "config",
        DiagnosticStatus::Pass,
        "Configuration loaded and validated",
        None,
    );
    report.add(
        "config.counts",
        "config",
        DiagnosticStatus::Pass,
        format!(
            "Project '{}' with {} hosts and {} services",
            config.project.name,
            config.hosts.len(),
            config.services.len()
        ),
        None,
    );

    if let Some(traefik) = &config.traefik {
        report.add(
            "config.traefik_hosts",
            "config",
            DiagnosticStatus::Pass,
            format!(
                "Traefik {} targets {} host(s): {}",
                traefik.image,
                config.traefik_host_names().len(),
                config.traefik_host_names().join(", ")
            ),
            None,
        );
    }

    let host_name_dupes = duplicate_values(config.hosts.iter().map(|host| host.name.as_str()));
    if host_name_dupes.is_empty() {
        report.add(
            "config.host_names",
            "config",
            DiagnosticStatus::Pass,
            "Host names are unique",
            None,
        );
    } else {
        report.add(
            "config.host_names",
            "config",
            DiagnosticStatus::Fail,
            format!("Duplicate host names: {}", host_name_dupes.join(", ")),
            Some("Each host must have a unique name".to_string()),
        );
    }

    let service_name_dupes =
        duplicate_values(config.services.iter().map(|service| service.name.as_str()));
    if service_name_dupes.is_empty() {
        report.add(
            "config.service_names",
            "config",
            DiagnosticStatus::Pass,
            "Service names are unique",
            None,
        );
    } else {
        report.add(
            "config.service_names",
            "config",
            DiagnosticStatus::Fail,
            format!("Duplicate service names: {}", service_name_dupes.join(", ")),
            Some("Each service must have a unique name".to_string()),
        );
    }

    report.add(
        "config.placement_labels",
        "config",
        DiagnosticStatus::Pass,
        "Traefik host references and service placement labels resolve",
        None,
    );

    let duplicate_public_addresses =
        duplicate_values(config.hosts.iter().map(|host| host.ssh_address()));
    if duplicate_public_addresses.is_empty() {
        report.add(
            "config.public_addresses",
            "config",
            DiagnosticStatus::Pass,
            "SSH addresses are unique",
            None,
        );
    } else {
        report.add(
            "config.public_addresses",
            "config",
            DiagnosticStatus::Warn,
            format!(
                "Multiple hosts share SSH addresses: {}",
                duplicate_public_addresses.join(", ")
            ),
            Some(
                "This may be intentional behind port differences, but verify host targeting."
                    .to_string(),
            ),
        );
    }

    let duplicate_internal_addresses =
        duplicate_values(config.hosts.iter().map(|host| host.internal_addr()));
    if duplicate_internal_addresses.is_empty() {
        report.add(
            "config.internal_addresses",
            "config",
            DiagnosticStatus::Pass,
            "Internal addresses are unique",
            None,
        );
    } else {
        report.add(
            "config.internal_addresses",
            "config",
            DiagnosticStatus::Warn,
            format!(
                "Multiple hosts share internal addresses: {}",
                duplicate_internal_addresses.join(", ")
            ),
            Some(
                "Verify Traefik routing and service-to-service traffic are unambiguous."
                    .to_string(),
            ),
        );
    }

    let empty_docker_sockets: Vec<_> = config
        .hosts
        .iter()
        .filter(|host| {
            host.docker_socket
                .as_deref()
                .is_some_and(|path| path.trim().is_empty())
        })
        .map(|host| host.name.clone())
        .collect();
    if empty_docker_sockets.is_empty() {
        report.add(
            "config.docker_sockets",
            "config",
            DiagnosticStatus::Pass,
            "Docker socket paths are valid",
            None,
        );
    } else {
        report.add(
            "config.docker_sockets",
            "config",
            DiagnosticStatus::Fail,
            format!(
                "Hosts with empty docker_socket values: {}",
                empty_docker_sockets.join(", ")
            ),
            Some("Remove the override or set a valid remote socket path.".to_string()),
        );
    }
}

async fn collect_ssh_and_docker_diagnostics(config: &Config, report: &mut DiagnosticReport) {
    for host in &config.hosts {
        let scope = format!("host:{}", host.name);
        let attempt = SshSession::connect_with_report(host);

        report.add(
            format!("ssh.tcp_connect.{}", host.name),
            scope.clone(),
            map_ssh_status(&attempt.report.tcp_connect.status),
            attempt.report.tcp_connect.message.clone(),
            attempt.report.tcp_connect.hint.clone(),
        );
        report.add(
            format!("ssh.handshake.{}", host.name),
            scope.clone(),
            map_ssh_status(&attempt.report.handshake.status),
            attempt.report.handshake.message.clone(),
            attempt.report.handshake.hint.clone(),
        );
        report.add(
            format!("ssh.host_key.{}", host.name),
            scope.clone(),
            map_ssh_status(&attempt.report.host_key.status),
            attempt.report.host_key.message.clone(),
            attempt.report.host_key.hint.clone(),
        );
        report.add(
            format!("ssh.authentication.{}", host.name),
            scope.clone(),
            map_ssh_status(&attempt.report.authentication.status),
            attempt.report.authentication.message.clone(),
            attempt.report.authentication.hint.clone(),
        );

        let Some(session) = attempt.session else {
            report.add(
                format!("ssh.ping.{}", host.name),
                scope.clone(),
                DiagnosticStatus::Fail,
                "Remote ping was not attempted because SSH connection did not complete",
                None,
            );
            report.add(
                format!("docker.connect.{}", host.name),
                scope.clone(),
                DiagnosticStatus::Fail,
                "Docker connectivity was not attempted because SSH connection did not complete",
                None,
            );
            continue;
        };

        match session.ping() {
            Ok(()) => report.add(
                format!("ssh.ping.{}", host.name),
                scope.clone(),
                DiagnosticStatus::Pass,
                "SSH ping succeeded",
                None,
            ),
            Err(e) => report.add(
                format!("ssh.ping.{}", host.name),
                scope.clone(),
                DiagnosticStatus::Fail,
                "SSH ping failed",
                Some(e.to_string()),
            ),
        }

        match DockerHost::connect_with_session(host, &session).await {
            Ok(_) => report.add(
                format!("docker.connect.{}", host.name),
                scope,
                DiagnosticStatus::Pass,
                "Docker ping succeeded over the SSH tunnel",
                None,
            ),
            Err(e) => report.add(
                format!("docker.connect.{}", host.name),
                scope,
                DiagnosticStatus::Fail,
                "Docker connectivity failed",
                Some(e.to_string()),
            ),
        }
    }
}

fn render_report(report: &DiagnosticReport) {
    output::header("Config");
    for check in report.checks.iter().filter(|check| check.scope == "config") {
        render_check(check);
    }

    output::header("Host Readiness");
    for check in report
        .checks
        .iter()
        .filter(|check| check.scope.starts_with("host:"))
    {
        render_check(check);
    }

    output::header("Summary");
    if report.has_failures() {
        output::error(&format!(
            "{} pass, {} warn, {} fail",
            report.summary.pass, report.summary.warn, report.summary.fail
        ));
        output::error("Deploy-time commands are not ready on all targets");
    } else {
        output::success(&format!(
            "{} pass, {} warn, {} fail",
            report.summary.pass, report.summary.warn, report.summary.fail
        ));
        output::success("Deploy-time commands are ready");
    }
}

fn render_check(check: &DiagnosticCheck) {
    match check.status {
        DiagnosticStatus::Pass => {
            output::success(&format!("[PASS] {}: {}", check.scope, check.message))
        }
        DiagnosticStatus::Warn => {
            output::warn(&format!("[WARN] {}: {}", check.scope, check.message))
        }
        DiagnosticStatus::Fail => {
            output::error(&format!("[FAIL] {}: {}", check.scope, check.message))
        }
    }

    if let Some(hint) = &check.hint
        && !matches!(check.status, DiagnosticStatus::Pass)
    {
        output::info(&format!("hint: {}", hint));
    }
}

fn map_ssh_status(status: &SshConnectStatus) -> DiagnosticStatus {
    match status {
        SshConnectStatus::Pass => DiagnosticStatus::Pass,
        SshConnectStatus::Fail => DiagnosticStatus::Fail,
    }
}

fn duplicate_values<'a>(values: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut counts = HashMap::<&str, usize>::new();
    for value in values {
        *counts.entry(value).or_insert(0) += 1;
    }

    let mut duplicates = counts
        .into_iter()
        .filter_map(|(value, count)| (count > 1).then(|| value.to_string()))
        .collect::<Vec<_>>();
    duplicates.sort();
    duplicates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_report_summary_counts() {
        let mut report = DiagnosticReport::new();
        report.add("a", "config", DiagnosticStatus::Pass, "ok", None);
        report.add("b", "config", DiagnosticStatus::Warn, "warn", None);
        report.add("c", "config", DiagnosticStatus::Fail, "fail", None);

        assert_eq!(report.summary.pass, 1);
        assert_eq!(report.summary.warn, 1);
        assert_eq!(report.summary.fail, 1);
        assert!(report.has_failures());
    }

    #[test]
    fn test_duplicate_values() {
        let duplicates = duplicate_values(["a", "b", "a", "c", "b"].into_iter());
        assert_eq!(duplicates, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn test_json_output_shape_is_stable() {
        let mut report = DiagnosticReport::new();
        report.add(
            "config.load",
            "config",
            DiagnosticStatus::Pass,
            "Configuration loaded",
            None,
        );

        let json = serde_json::to_value(&report).unwrap();
        assert!(json.get("summary").is_some());
        assert!(json.get("checks").is_some());
        assert_eq!(json["checks"][0]["status"], "pass");
        assert_eq!(json["checks"][0]["id"], "config.load");
    }

    #[test]
    fn test_warnings_do_not_count_as_failures() {
        let mut report = DiagnosticReport::new();
        report.add("warn", "config", DiagnosticStatus::Warn, "warn", None);
        assert!(!report.has_failures());
    }
}
