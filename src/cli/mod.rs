use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub mod output;

#[derive(Parser, Debug)]
#[command(name = "korgi", version, about = "Docker orchestration across multiple hosts via SSH")]
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
