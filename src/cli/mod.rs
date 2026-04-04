use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub mod output;

#[derive(Parser, Debug)]
#[command(
    name = "korgi",
    version,
    about = "Docker orchestration across multiple hosts via SSH"
)]
pub struct Cli {
    /// Path to korgi.toml config file
    #[arg(long, short, default_value = "korgi.toml")]
    pub config: PathBuf,

    /// Environment overlay (loads korgi.{env}.toml)
    #[arg(long, short)]
    pub env: Option<String>,

    /// Output as JSON
    #[arg(long, global = true)]
    pub json: bool,

    /// Skip confirmation prompts
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Scaffold a new korgi.toml configuration file
    Init,

    /// Validate configuration and test SSH connectivity
    Check,

    /// Show running containers across hosts
    Status {
        /// Filter by service name
        #[arg(long)]
        service: Option<String>,
    },

    /// Deploy services (or a specific service)
    Deploy {
        /// Deploy only this service
        #[arg(long)]
        service: Option<String>,

        /// Override the image for deployment
        #[arg(long)]
        image: Option<String>,

        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
    },

    /// Roll back a service to its previous generation
    Rollback {
        /// Service to roll back
        #[arg(long, required = true)]
        service: String,
    },

    /// Scale a service to N replicas
    Scale {
        /// Service to scale
        #[arg(long, required = true)]
        service: String,

        /// Target replica count
        count: u32,
    },

    /// Manage Traefik instances
    Traefik {
        #[command(subcommand)]
        action: TraefikAction,
    },

    /// Execute a command in a running container
    Exec {
        /// Service name
        #[arg(long, required = true)]
        service: String,

        /// Command and arguments
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },

    /// Tail logs from a service
    Logs {
        /// Service name
        #[arg(long, required = true)]
        service: String,

        /// Follow log output
        #[arg(long, short)]
        follow: bool,
    },

    /// Stop and remove containers
    Destroy {
        /// Only destroy this service (otherwise all)
        #[arg(long)]
        service: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum TraefikAction {
    /// Deploy Traefik to configured hosts
    Deploy,
    /// Show Traefik status
    Status,
    /// Tail Traefik logs
    Logs {
        /// Follow log output
        #[arg(long, short)]
        follow: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::parse_from(args)
    }

    #[test]
    fn test_init() {
        let cli = parse(&["korgi", "init"]);
        assert!(matches!(cli.command, Commands::Init));
        assert_eq!(cli.config, std::path::PathBuf::from("korgi.toml"));
    }

    #[test]
    fn test_check() {
        let cli = parse(&["korgi", "check"]);
        assert!(matches!(cli.command, Commands::Check));
    }

    #[test]
    fn test_status_no_filter() {
        let cli = parse(&["korgi", "status"]);
        match &cli.command {
            Commands::Status { service } => assert!(service.is_none()),
            _ => panic!("expected Status"),
        }
    }

    #[test]
    fn test_status_with_service() {
        let cli = parse(&["korgi", "status", "--service", "api"]);
        match &cli.command {
            Commands::Status { service } => assert_eq!(service.as_deref(), Some("api")),
            _ => panic!("expected Status"),
        }
    }

    #[test]
    fn test_deploy_minimal() {
        let cli = parse(&["korgi", "deploy"]);
        match &cli.command {
            Commands::Deploy {
                service,
                image,
                dry_run,
            } => {
                assert!(service.is_none());
                assert!(image.is_none());
                assert!(!dry_run);
            }
            _ => panic!("expected Deploy"),
        }
    }

    #[test]
    fn test_deploy_with_options() {
        let cli = parse(&[
            "korgi",
            "deploy",
            "--service",
            "api",
            "--image",
            "myapp:v2",
            "--dry-run",
        ]);
        match &cli.command {
            Commands::Deploy {
                service,
                image,
                dry_run,
            } => {
                assert_eq!(service.as_deref(), Some("api"));
                assert_eq!(image.as_deref(), Some("myapp:v2"));
                assert!(dry_run);
            }
            _ => panic!("expected Deploy"),
        }
    }

    #[test]
    fn test_rollback() {
        let cli = parse(&["korgi", "rollback", "--service", "api"]);
        match &cli.command {
            Commands::Rollback { service } => assert_eq!(service, "api"),
            _ => panic!("expected Rollback"),
        }
    }

    #[test]
    fn test_scale() {
        let cli = parse(&["korgi", "scale", "--service", "api", "5"]);
        match &cli.command {
            Commands::Scale { service, count } => {
                assert_eq!(service, "api");
                assert_eq!(*count, 5);
            }
            _ => panic!("expected Scale"),
        }
    }

    #[test]
    fn test_traefik_deploy() {
        let cli = parse(&["korgi", "traefik", "deploy"]);
        match &cli.command {
            Commands::Traefik { action } => {
                assert!(matches!(action, TraefikAction::Deploy));
            }
            _ => panic!("expected Traefik"),
        }
    }

    #[test]
    fn test_traefik_status() {
        let cli = parse(&["korgi", "traefik", "status"]);
        match &cli.command {
            Commands::Traefik { action } => {
                assert!(matches!(action, TraefikAction::Status));
            }
            _ => panic!("expected Traefik"),
        }
    }

    #[test]
    fn test_traefik_logs() {
        let cli = parse(&["korgi", "traefik", "logs", "--follow"]);
        match &cli.command {
            Commands::Traefik { action } => match action {
                TraefikAction::Logs { follow } => assert!(follow),
                _ => panic!("expected Logs"),
            },
            _ => panic!("expected Traefik"),
        }
    }

    #[test]
    fn test_exec() {
        let cli = parse(&[
            "korgi",
            "exec",
            "--service",
            "api",
            "--",
            "sh",
            "-c",
            "echo hello",
        ]);
        match &cli.command {
            Commands::Exec { service, cmd } => {
                assert_eq!(service, "api");
                assert_eq!(cmd, &vec!["sh", "-c", "echo hello"]);
            }
            _ => panic!("expected Exec"),
        }
    }

    #[test]
    fn test_logs() {
        let cli = parse(&["korgi", "logs", "--service", "api", "--follow"]);
        match &cli.command {
            Commands::Logs { service, follow } => {
                assert_eq!(service, "api");
                assert!(follow);
            }
            _ => panic!("expected Logs"),
        }
    }

    #[test]
    fn test_destroy_all() {
        let cli = parse(&["korgi", "destroy"]);
        match &cli.command {
            Commands::Destroy { service } => assert!(service.is_none()),
            _ => panic!("expected Destroy"),
        }
    }

    #[test]
    fn test_destroy_service() {
        let cli = parse(&["korgi", "destroy", "--service", "api"]);
        match &cli.command {
            Commands::Destroy { service } => assert_eq!(service.as_deref(), Some("api")),
            _ => panic!("expected Destroy"),
        }
    }

    #[test]
    fn test_global_config() {
        let cli = parse(&["korgi", "--config", "/path/to/config.toml", "check"]);
        assert_eq!(cli.config, std::path::PathBuf::from("/path/to/config.toml"));
    }

    #[test]
    fn test_global_env() {
        let cli = parse(&["korgi", "--env", "staging", "deploy"]);
        assert_eq!(cli.env.as_deref(), Some("staging"));
    }

    #[test]
    fn test_global_json() {
        let cli = parse(&["korgi", "status", "--json"]);
        assert!(cli.json);
    }

    #[test]
    fn test_json_works_on_any_command() {
        let cli = parse(&["korgi", "deploy", "--json"]);
        assert!(cli.json);
    }

    #[test]
    fn test_yes_flag() {
        let cli = parse(&["korgi", "-y", "deploy"]);
        assert!(cli.yes);
    }

    #[test]
    fn test_yes_long_flag() {
        let cli = parse(&["korgi", "--yes", "destroy"]);
        assert!(cli.yes);
    }

    #[test]
    fn test_no_yes_by_default() {
        let cli = parse(&["korgi", "deploy"]);
        assert!(!cli.yes);
    }
}
