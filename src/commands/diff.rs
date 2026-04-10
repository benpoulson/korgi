use anyhow::Result;
use std::collections::HashMap;

use crate::cli::output;
use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::docker::traits::DockerHostApi;
use crate::orchestrator::diff::{DiffReport, ServiceDiffAction, diff_services};

pub async fn run(
    config: &Config,
    service_filter: Option<&str>,
    image_override: Option<&str>,
    docker_hosts: &HashMap<String, DockerHost>,
    json_output: bool,
) -> Result<()> {
    run_with_hosts(
        config,
        service_filter,
        image_override,
        docker_hosts,
        json_output,
    )
    .await
}

async fn run_with_hosts<D: DockerHostApi>(
    config: &Config,
    service_filter: Option<&str>,
    image_override: Option<&str>,
    docker_hosts: &HashMap<String, D>,
    json_output: bool,
) -> Result<()> {
    let report = diff_services(config, service_filter, image_override, docker_hosts).await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    render_report(&report);
    Ok(())
}

fn render_report(report: &DiffReport) {
    output::header("Diff");
    for service in &report.services {
        let marker = match service.action {
            ServiceDiffAction::Create => "CREATE",
            ServiceDiffAction::Update => "UPDATE",
            ServiceDiffAction::Replace => "REPLACE",
            ServiceDiffAction::Noop => "NOOP",
        };
        output::info(&format!("{} {}", marker, service.service));
        output::info(&format!(
            "current generation: {}",
            service
                .current_running_generation
                .map(|generation| generation.to_string())
                .unwrap_or_else(|| "none".to_string())
        ));
        output::info(&format!(
            "planned generation: {}",
            service.planned_generation
        ));
        output::info(&format!(
            "placements: {} -> {}",
            format_placements(&service.current_placements),
            format_placements(&service.planned_placements)
        ));

        if service.field_changes.is_empty() {
            output::success("no config changes");
        } else {
            for change in &service.field_changes {
                output::info(&format!(
                    "{}: {} -> {}",
                    change.field, change.before, change.after
                ));
            }
        }
    }

    output::header("Summary");
    if report.summary.changed_services == 0 {
        output::success("No changes detected");
    } else {
        output::info(&format!(
            "{} changed, {} unchanged, {} planned containers",
            report.summary.changed_services,
            report.summary.unchanged_services,
            report.summary.total_planned_containers
        ));
    }
}

fn format_placements(placements: &[crate::orchestrator::diff::PlacementPlan]) -> String {
    if placements.is_empty() {
        return "none".to_string();
    }
    placements
        .iter()
        .map(|placement| format!("{}@{}", placement.instance, placement.host))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::*;
    use crate::docker::mock::tests::MockDockerHost;

    fn test_config() -> Config {
        Config {
            project: ProjectConfig {
                name: "myapp".to_string(),
                secrets: None,
            },
            registries: vec![],
            hosts: vec![{
                let mut host = HostConfig::test_host("web1", "10.0.0.1");
                host.labels = vec!["web".to_string()];
                host
            }],
            traefik: None,
            services: vec![
                ServiceConfig::test_service("api", "myapp/api:v1"),
                ServiceConfig::test_service("worker", "myapp/worker:v1"),
            ],
        }
    }

    fn mock_hosts() -> HashMap<String, MockDockerHost> {
        let mut hosts = HashMap::new();
        hosts.insert("web1".to_string(), MockDockerHost::new("web1"));
        hosts
    }

    #[tokio::test]
    async fn test_diff_all_services_json() {
        let config = test_config();
        let hosts = mock_hosts();
        let report = diff_services(&config, None, None, &hosts).await.unwrap();
        assert_eq!(report.services.len(), 2);
    }

    #[tokio::test]
    async fn test_diff_service_filter() {
        let config = test_config();
        let hosts = mock_hosts();
        let report = diff_services(&config, Some("api"), None, &hosts)
            .await
            .unwrap();
        assert_eq!(report.services.len(), 1);
        assert_eq!(report.services[0].service, "api");
    }
}
