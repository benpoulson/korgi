pub mod interpolate;
pub mod merge;
pub mod types;

pub use types::Config;

use std::path::PathBuf;

/// Resolve and load configuration, applying environment overlay and interpolation.
pub fn load_config(config_path: &PathBuf, env: Option<&str>) -> anyhow::Result<Config> {
    let merged_toml = merge::load_and_merge(config_path, env)?;
    let sys_env = interpolate::system_env();
    let interpolated = interpolate::interpolate_str(&merged_toml, &sys_env)?;
    let config: Config = toml::from_str(&interpolated)
        .map_err(|e| anyhow::anyhow!("Failed to parse config: {}", e))?;
    config.validate()?;
    Ok(config)
}
