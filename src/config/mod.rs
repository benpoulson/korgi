pub mod interpolate;
pub mod merge;
pub mod types;

pub use types::Config;

/// Resolve and load configuration, applying environment overlay and interpolation.
pub fn load_config(config_path: &std::path::Path, env: Option<&str>) -> anyhow::Result<Config> {
    let merged_toml = merge::load_and_merge(config_path, env)?;
    let sys_env = interpolate::system_env();
    let interpolated = interpolate::interpolate_str(&merged_toml, &sys_env)?;
    let config: Config = toml::from_str(&interpolated)
        .map_err(|e| anyhow::anyhow!("Failed to parse config: {}", e))?;
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_load_config_basic() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("korgi.toml");
        std::fs::write(&path, r#"
            [project]
            name = "test"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
            [[services]]
            name = "web"
            image = "web:latest"
        "#).unwrap();

        let config = load_config(&path, None).unwrap();
        assert_eq!(config.project.name, "test");
    }

    #[test]
    fn test_load_config_with_env_overlay() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().join("korgi.toml");
        let overlay = dir.path().join("korgi.staging.toml");

        std::fs::write(&base, r#"
            [project]
            name = "prod"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
            [[services]]
            name = "web"
            image = "web:v1"
            replicas = 3
        "#).unwrap();

        std::fs::write(&overlay, r#"
            [[services]]
            name = "web"
            image = "web:v1-staging"
            replicas = 1
        "#).unwrap();

        let config = load_config(&base, Some("staging")).unwrap();
        // Overlay replaces services array
        assert_eq!(config.services[0].replicas, 1);
        assert_eq!(config.services[0].image, "web:v1-staging");
    }

    #[test]
    fn test_load_config_with_interpolation() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("korgi.toml");

        // SAFETY: This test runs single-threaded and we clean up the var after
        unsafe { std::env::set_var("KORGI_TEST_DB_HOST", "db.example.com"); }

        std::fs::write(&path, r#"
            [project]
            name = "test"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
            [[services]]
            name = "web"
            image = "web:latest"
            [services.env]
            DATABASE_URL = "postgres://${KORGI_TEST_DB_HOST}/mydb"
        "#).unwrap();

        let config = load_config(&path, None).unwrap();
        assert_eq!(
            config.services[0].env.get("DATABASE_URL").unwrap(),
            "postgres://db.example.com/mydb"
        );

        // SAFETY: cleaning up test env var
        unsafe { std::env::remove_var("KORGI_TEST_DB_HOST"); }
    }

    #[test]
    fn test_load_config_missing_env_var_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("korgi.toml");

        std::fs::write(&path, r#"
            [project]
            name = "test"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
            [[services]]
            name = "web"
            image = "web:latest"
            [services.env]
            SECRET = "${DEFINITELY_NOT_A_REAL_ENV_VAR_12345}"
        "#).unwrap();

        let result = load_config(&path, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_config_nonexistent_file() {
        let result = load_config(&PathBuf::from("/nonexistent/korgi.toml"), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_config_invalid_toml() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("korgi.toml");
        std::fs::write(&path, "this is not { valid toml").unwrap();

        let result = load_config(&path, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_config_valid_toml_invalid_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("korgi.toml");
        // Valid TOML but fails validation (no hosts)
        std::fs::write(&path, r#"
            [project]
            name = "test"
        "#).unwrap();

        let result = load_config(&path, None);
        assert!(result.is_err());
    }
}
