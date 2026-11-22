//! Syncs Traefik dynamic configuration after deploy/scale/rollback.
//!
//! The implementation lives in [`crate::orchestrator::traefik_sync`] so the
//! orchestrator can order syncs relative to starting and stopping containers.

pub use crate::orchestrator::traefik_sync::{sync_traefik_config, sync_traefik_config_excluding};
