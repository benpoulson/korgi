use anyhow::Result;
use clap::Parser;
use std::collections::HashMap;
use tracing_subscriber::EnvFilter;

use korgi::cli::output;
use korgi::cli::{Cli, Commands, TraefikAction};
use korgi::config;
use korgi::docker::DockerHost;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();

    match &cli.command {
        Commands::Init => {
            korgi::commands::init::run(&cli.config)?;
        }
        _ => {
            run_with_config(&cli).await?;
        }
    }

    Ok(())
}

async fn run_with_config(cli: &Cli) -> Result<()> {
    let cfg = config::load_config(&cli.config, cli.env.as_deref())?;

    match &cli.command {
        Commands::Init => unreachable!(),

        Commands::Check => {
            korgi::commands::check::run(&cfg).await?;
        }

        Commands::Status { service } => {
            let docker_hosts = connect_docker_hosts(&cfg).await?;
            korgi::commands::status::run(&cfg, service.as_deref(), &docker_hosts, cli.json).await?;
        }

        Commands::Deploy {
            service,
            image,
            dry_run,
        } => {
            let docker_hosts = connect_docker_hosts(&cfg).await?;
            korgi::commands::deploy::run(
                &cfg,
                service.as_deref(),
                image.as_deref(),
                *dry_run,
                cli.yes,
                &docker_hosts,
            )
            .await?;
        }

        Commands::Rollback { service } => {
            let docker_hosts = connect_docker_hosts(&cfg).await?;
            korgi::commands::rollback::run(&cfg, service, cli.yes, &docker_hosts).await?;
        }

        Commands::Scale { service, count } => {
            let docker_hosts = connect_docker_hosts(&cfg).await?;
            korgi::commands::scale::run(&cfg, service, *count, cli.yes, &docker_hosts).await?;
        }

        Commands::Traefik { action } => {
            let docker_hosts = connect_docker_hosts(&cfg).await?;
            match action {
                TraefikAction::Deploy => {
                    korgi::commands::traefik::deploy(&cfg, &docker_hosts).await?;
                }
                TraefikAction::Status => {
                    korgi::commands::traefik::status(&cfg, &docker_hosts).await?;
                }
                TraefikAction::Logs { follow } => {
                    korgi::commands::traefik::logs(&cfg, &docker_hosts, *follow).await?;
                }
            }
        }

        Commands::Exec { service, cmd } => {
            let docker_hosts = connect_docker_hosts(&cfg).await?;
            korgi::commands::exec::run(&cfg, service, cmd, &docker_hosts).await?;
        }

        Commands::Logs { service, follow } => {
            let docker_hosts = connect_docker_hosts(&cfg).await?;
            korgi::commands::logs::run(&cfg, service, *follow, &docker_hosts).await?;
        }

        Commands::Destroy { service } => {
            let docker_hosts = connect_docker_hosts(&cfg).await?;
            korgi::commands::destroy::run(&cfg, service.as_deref(), cli.yes, &docker_hosts).await?;
        }
    }

    Ok(())
}

/// Connect to Docker on all configured hosts.
async fn connect_docker_hosts(
    cfg: &korgi::config::types::Config,
) -> Result<HashMap<String, DockerHost>> {
    let pb = output::spinner("Connecting to Docker on all hosts...");
    let mut hosts = HashMap::new();

    for host in &cfg.hosts {
        let docker = DockerHost::connect(host).await?;
        hosts.insert(host.name.clone(), docker);
    }

    pb.finish_and_clear();
    Ok(hosts)
}
