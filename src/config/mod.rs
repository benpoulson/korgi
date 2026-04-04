pub mod interpolate;
pub mod merge;
pub mod types;

pub use types::Config;

/// Resolve and load configuration, applying environment overlay and interpolation.
pub fn load_config(config_path: &std::path::Path, env: Option<&str>) -> anyhow::Result<Config> {
    let merged_toml = merge::load_and_merge(config_path, env)?;

    // Build env map: secrets file (if configured) overlaid with system env vars.
    // System env takes precedence over the secrets file.
    let mut env_map = load_secrets_from_raw_toml(&merged_toml, config_path)?;
    env_map.extend(interpolate::system_env());

    let interpolated = interpolate::interpolate_str(&merged_toml, &env_map)?;
    let config: Config = toml::from_str(&interpolated)
        .map_err(|e| anyhow::anyhow!("Failed to parse config: {}", e))?;
    config.validate()?;
    Ok(config)
}

/// Extract project.secrets path from raw TOML and load the secrets file if it exists.
/// The secrets file format is KEY=VALUE per line (blank lines and # comments ignored).
fn load_secrets_from_raw_toml(
    raw_toml: &str,
    config_path: &std::path::Path,
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let mut secrets = std::collections::HashMap::new();

    // Quick parse to extract just the secrets path -- don't interpolate yet
    let raw: toml::Value = raw_toml
        .parse()
        .map_err(|e| anyhow::anyhow!("Failed to parse config: {}", e))?;

    let secrets_path = raw
        .get("project")
        .and_then(|p| p.get("secrets"))
        .and_then(|s| s.as_str());

    let Some(secrets_path) = secrets_path else {
        return Ok(secrets);
    };

    // Resolve relative to the config file's directory
    let secrets_file = if std::path::Path::new(secrets_path).is_absolute() {
        std::path::PathBuf::from(secrets_path)
    } else {
        config_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join(secrets_path)
    };

    let content = match std::fs::read_to_string(&secrets_file) {
        Ok(c) => {
            tracing::debug!("Loaded secrets from {}", secrets_file.display());
            c
        }
        Err(e) => {
            tracing::warn!(
                "Secrets file '{}' not found ({}). Variables will resolve from system env only.",
                secrets_file.display(),
                e
            );
            return Ok(secrets);
        }
    };

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            secrets.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    Ok(secrets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_load_config_basic() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("korgi.toml");
        std::fs::write(
            &path,
            r#"
            [project]
            name = "test"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
            [[services]]
            name = "web"
            image = "web:latest"
        "#,
        )
        .unwrap();

        let config = load_config(&path, None).unwrap();
        assert_eq!(config.project.name, "test");
    }

    #[test]
    fn test_load_config_with_env_overlay() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().join("korgi.toml");
        let overlay = dir.path().join("korgi.staging.toml");

        std::fs::write(
            &base,
            r#"
            [project]
            name = "prod"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
            [[services]]
            name = "web"
            image = "web:v1"
            replicas = 3
        "#,
        )
        .unwrap();

        std::fs::write(
            &overlay,
            r#"
            [[services]]
            name = "web"
            image = "web:v1-staging"
            replicas = 1
        "#,
        )
        .unwrap();

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
        unsafe {
            std::env::set_var("KORGI_TEST_DB_HOST", "db.example.com");
        }

        std::fs::write(
            &path,
            r#"
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
        "#,
        )
        .unwrap();

        let config = load_config(&path, None).unwrap();
        assert_eq!(
            config.services[0].env.get("DATABASE_URL").unwrap(),
            "postgres://db.example.com/mydb"
        );

        // SAFETY: cleaning up test env var
        unsafe {
            std::env::remove_var("KORGI_TEST_DB_HOST");
        }
    }

    #[test]
    fn test_load_config_missing_env_var_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("korgi.toml");

        std::fs::write(
            &path,
            r#"
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
        "#,
        )
        .unwrap();

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
        std::fs::write(
            &path,
            r#"
            [project]
            name = "test"
        "#,
        )
        .unwrap();

        let result = load_config(&path, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_config_with_secrets_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("korgi.toml");
        let secrets = dir.path().join("secrets");

        std::fs::write(
            &secrets,
            "DB_PASSWORD=hunter2\n# comment\nJWT_SECRET=supersecret\n",
        )
        .unwrap();

        std::fs::write(
            &path,
            r#"
            [project]
            name = "test"
            secrets = "secrets"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
            [[services]]
            name = "web"
            image = "web:latest"
            [services.env]
            DB_PASSWORD = "${DB_PASSWORD}"
            JWT_SECRET = "${JWT_SECRET}"
        "#,
        )
        .unwrap();

        let config = load_config(&path, None).unwrap();
        assert_eq!(
            config.services[0].env.get("DB_PASSWORD").unwrap(),
            "hunter2"
        );
        assert_eq!(
            config.services[0].env.get("JWT_SECRET").unwrap(),
            "supersecret"
        );
    }

    #[test]
    fn test_secrets_file_system_env_takes_precedence() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("korgi.toml");
        let secrets = dir.path().join("secrets");

        std::fs::write(&secrets, "KORGI_TEST_OVERRIDE=from_file\n").unwrap();

        // SAFETY: test env var
        unsafe {
            std::env::set_var("KORGI_TEST_OVERRIDE", "from_env");
        }

        std::fs::write(
            &path,
            r#"
            [project]
            name = "test"
            secrets = "secrets"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
            [[services]]
            name = "web"
            image = "web:latest"
            [services.env]
            VAL = "${KORGI_TEST_OVERRIDE}"
        "#,
        )
        .unwrap();

        let config = load_config(&path, None).unwrap();
        assert_eq!(config.services[0].env.get("VAL").unwrap(), "from_env");

        unsafe {
            std::env::remove_var("KORGI_TEST_OVERRIDE");
        }
    }

    #[test]
    fn test_secrets_file_missing_is_ok() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("korgi.toml");

        std::fs::write(
            &path,
            r#"
            [project]
            name = "test"
            secrets = "nonexistent"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
        "#,
        )
        .unwrap();

        // Missing secrets file is fine -- only fails if a ${VAR} can't resolve
        let result = load_config(&path, None);
        assert!(result.is_ok());
    }
}
