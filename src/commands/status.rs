use anyhow::Result;
use std::collections::HashMap;
use tabled::{Table, Tabled};

use crate::cli::output;
use crate::config::types::Config;
use crate::docker::host::DockerHost;
use crate::orchestrator::state::LiveState;

#[derive(Tabled)]
struct ContainerRow {
    #[tabled(rename = "SERVICE")]
    service: String,
    #[tabled(rename = "HOST")]
    host: String,
    #[tabled(rename = "CONTAINER")]
    name: String,
    #[tabled(rename = "GEN")]
    generation: u64,
    #[tabled(rename = "STATE")]
    state: String,
    #[tabled(rename = "HEALTH")]
    health: String,
    #[tabled(rename = "IMAGE")]
    image: String,
}

pub async fn run(
    config: &Config,
    service_filter: Option<&str>,
    docker_hosts: &HashMap<String, DockerHost>,
    json_output: bool,
) -> Result<()> {
    let state = LiveState::query(docker_hosts, &config.project.name).await?;

    let containers: Vec<_> = if let Some(svc) = service_filter {
        state.service_containers(svc).into_iter().collect()
    } else {
        state.containers.iter().collect()
    };

    if json_output {
        let json_data: Vec<_> = containers
            .iter()
            .map(|c| {
                serde_json::json!({
                    "service": c.service,
                    "host": c.host_name,
                    "name": c.name,
                    "generation": c.generation,
                    "instance": c.instance,
                    "state": c.state,
                    "health": c.health,
                    "image": c.image,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_data)?);
        return Ok(());
    }

    if containers.is_empty() {
        output::info("No containers found");
        return Ok(());
    }

    let rows: Vec<ContainerRow> = containers
        .iter()
        .map(|c| ContainerRow {
            service: c.service.clone(),
            host: c.host_name.clone(),
            name: c.name.clone(),
            generation: c.generation,
            state: c.state.clone(),
            health: c.health.clone().unwrap_or("-".to_string()),
            image: c.image.clone(),
        })
        .collect();

    let table = Table::new(rows).to_string();
    println!("{}", table);

    // Summary
    let services = state.services();
    let running_count = containers.iter().filter(|c| c.state == "running").count();
    output::info(&format!(
        "{} containers across {} services ({} running)",
        containers.len(),
        services.len(),
        running_count
    ));

    Ok(())
}
