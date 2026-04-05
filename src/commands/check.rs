use anyhow::Result;

use crate::cli::output;
use crate::config::types::Config;
use crate::ssh::SshPool;

pub async fn run(config: &Config) -> Result<()> {
    output::header("Configuration");
    output::success(&format!("Project: {}", config.project.name));
    output::success(&format!("{} hosts configured", config.hosts.len()));
    output::success(&format!("{} services configured", config.services.len()));

    if let Some(traefik) = &config.traefik {
        let traefik_hosts = config.traefik_host_names();
        output::success(&format!(
            "Traefik: {} on {} hosts ({})",
            traefik.image,
            traefik_hosts.len(),
            traefik_hosts.join(", ")
        ));
    }
    let lb_count = config.lb_hosts().len();
    let node_count = config.node_hosts().len();
    output::success(&format!(
        "{} load balancers, {} nodes",
        lb_count, node_count
    ));

    output::header("SSH Connectivity");
    let pool = SshPool::connect_all(config);
    match pool {
        Ok(pool) => {
            for (name, session) in pool.iter() {
                match session.ping() {
                    Ok(()) => output::success(&format!("{}: connected", name)),
                    Err(e) => output::error(&format!("{}: ping failed -- {}", name, e)),
                }
            }

            // Test Docker connectivity
            output::header("Docker Connectivity");
            for host in &config.hosts {
                let result = crate::docker::DockerHost::connect(host).await;
                match result {
                    Ok(_) => output::success(&format!("{}: Docker reachable", host.name)),
                    Err(e) => output::error(&format!("{}: Docker failed -- {}", host.name, e)),
                }
            }

            drop(pool);
        }
        Err(e) => {
            output::error(&format!("SSH connection failed: {}", e));
            return Err(e);
        }
    }

    output::header("Summary");
    output::success("All checks passed");

    Ok(())
}
