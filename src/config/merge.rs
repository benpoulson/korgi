use toml::Value;

/// Deep-merge two TOML values. Values from `overlay` take precedence.
/// Tables are merged recursively; arrays are replaced (not appended).
pub fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Table(base_table), Value::Table(overlay_table)) => {
            for (key, overlay_val) in overlay_table {
                match base_table.get_mut(&key) {
                    Some(base_val) => deep_merge(base_val, overlay_val),
                    None => {
                        base_table.insert(key, overlay_val);
                    }
                }
            }
        }
        (base, overlay) => {
            *base = overlay;
        }
    }
}

/// Load a base config file and optionally merge an environment overlay.
/// The overlay file is expected at `{base_stem}.{env}.toml`.
pub fn load_and_merge(
    base_path: &std::path::Path,
    env_name: Option<&str>,
) -> anyhow::Result<String> {
    let base_content = std::fs::read_to_string(base_path).map_err(|e| {
        anyhow::anyhow!("Failed to read config file {}: {}", base_path.display(), e)
    })?;

    let Some(env_name) = env_name else {
        return Ok(base_content);
    };

    let stem = base_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("korgi");
    let parent = base_path.parent().unwrap_or(std::path::Path::new("."));
    let overlay_path = parent.join(format!("{}.{}.toml", stem, env_name));

    if !overlay_path.exists() {
        anyhow::bail!(
            "Environment overlay file not found: {}",
            overlay_path.display()
        );
    }

    let overlay_content = std::fs::read_to_string(&overlay_path).map_err(|e| {
        anyhow::anyhow!(
            "Failed to read overlay file {}: {}",
            overlay_path.display(),
            e
        )
    })?;

    let mut base_val: Value = base_content
        .parse()
        .map_err(|e| anyhow::anyhow!("Failed to parse base config: {}", e))?;
    let overlay_val: Value = overlay_content
        .parse()
        .map_err(|e| anyhow::anyhow!("Failed to parse overlay config: {}", e))?;

    deep_merge(&mut base_val, overlay_val);

    Ok(toml::to_string(&base_val)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deep_merge_simple() {
        let mut base: Value = toml::from_str(
            r#"
            [project]
            name = "myapp"
            [traefik]
            image = "traefik:v3.1"
        "#,
        )
        .unwrap();

        let overlay: Value = toml::from_str(
            r#"
            [traefik]
            image = "traefik:v3.2"
        "#,
        )
        .unwrap();

        deep_merge(&mut base, overlay);

        assert_eq!(base["project"]["name"].as_str().unwrap(), "myapp");
        assert_eq!(base["traefik"]["image"].as_str().unwrap(), "traefik:v3.2");
    }

    #[test]
    fn test_deep_merge_nested() {
        let mut base: Value = toml::from_str(
            r#"
            [project]
            name = "myapp"
            [[services]]
            name = "api"
            image = "api:v1"
        "#,
        )
        .unwrap();

        let overlay: Value = toml::from_str(
            r#"
            [[services]]
            name = "api"
            image = "api:v2"
        "#,
        )
        .unwrap();

        deep_merge(&mut base, overlay);

        // Arrays are replaced, not merged
        let services = base["services"].as_array().unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0]["image"].as_str().unwrap(), "api:v2");
    }

    #[test]
    fn test_deep_merge_add_new_key() {
        let mut base: Value = toml::from_str(
            r#"
            [project]
            name = "myapp"
        "#,
        )
        .unwrap();

        let overlay: Value = toml::from_str(
            r#"
            [project]
            name = "myapp"
            [traefik]
            image = "traefik:v3.2"
        "#,
        )
        .unwrap();

        deep_merge(&mut base, overlay);

        assert_eq!(base["traefik"]["image"].as_str().unwrap(), "traefik:v3.2");
    }

    #[test]
    fn test_deep_merge_preserves_untouched_keys() {
        let mut base: Value = toml::from_str(
            r#"
            [project]
            name = "myapp"
            [[hosts]]
            name = "web1"
            address = "10.0.0.1"
            [[services]]
            name = "api"
            image = "api:v1"
            replicas = 3
        "#,
        )
        .unwrap();

        let overlay: Value = toml::from_str(
            r#"
            [project]
            name = "staging-app"
        "#,
        )
        .unwrap();

        deep_merge(&mut base, overlay);

        // Overlay changed project name
        assert_eq!(base["project"]["name"].as_str().unwrap(), "staging-app");
        // Hosts and services untouched
        assert_eq!(base["hosts"].as_array().unwrap().len(), 1);
        assert_eq!(base["services"].as_array().unwrap().len(), 1);
        assert_eq!(
            base["services"].as_array().unwrap()[0]["replicas"]
                .as_integer()
                .unwrap(),
            3
        );
    }

    #[test]
    fn test_deep_merge_scalar_override() {
        let mut base: Value = toml::from_str(
            r#"
            value = 42
        "#,
        )
        .unwrap();

        let overlay: Value = toml::from_str(
            r#"
            value = 99
        "#,
        )
        .unwrap();

        deep_merge(&mut base, overlay);
        assert_eq!(base["value"].as_integer().unwrap(), 99);
    }

    #[test]
    fn test_deep_merge_type_change() {
        let mut base: Value = toml::from_str(
            r#"
            value = "string"
        "#,
        )
        .unwrap();

        let overlay: Value = toml::from_str(
            r#"
            value = 42
        "#,
        )
        .unwrap();

        deep_merge(&mut base, overlay);
        assert_eq!(base["value"].as_integer().unwrap(), 42);
    }

    #[test]
    fn test_deep_merge_deeply_nested() {
        let mut base: Value = toml::from_str(
            r#"
            [a]
            [a.b]
            [a.b.c]
            value = "original"
            other = "kept"
        "#,
        )
        .unwrap();

        let overlay: Value = toml::from_str(
            r#"
            [a]
            [a.b]
            [a.b.c]
            value = "changed"
        "#,
        )
        .unwrap();

        deep_merge(&mut base, overlay);
        assert_eq!(base["a"]["b"]["c"]["value"].as_str().unwrap(), "changed");
        assert_eq!(base["a"]["b"]["c"]["other"].as_str().unwrap(), "kept");
    }

    #[test]
    fn test_load_and_merge_base_only() {
        let dir = tempfile::TempDir::new().unwrap();
        let base_path = dir.path().join("korgi.toml");
        std::fs::write(
            &base_path,
            r#"
            [project]
            name = "app"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
        "#,
        )
        .unwrap();

        let result = load_and_merge(&base_path, None).unwrap();
        assert!(result.contains("app"));
    }

    #[test]
    fn test_load_and_merge_with_overlay() {
        let dir = tempfile::TempDir::new().unwrap();
        let base_path = dir.path().join("korgi.toml");
        let overlay_path = dir.path().join("korgi.staging.toml");

        std::fs::write(
            &base_path,
            r#"
            [project]
            name = "app"
            [[hosts]]
            name = "h1"
            address = "1.2.3.4"
        "#,
        )
        .unwrap();

        std::fs::write(
            &overlay_path,
            r#"
            [project]
            name = "staging-app"
        "#,
        )
        .unwrap();

        let result = load_and_merge(&base_path, Some("staging")).unwrap();
        // The merged result should have the staging name
        let parsed: Value = result.parse().unwrap();
        assert_eq!(parsed["project"]["name"].as_str().unwrap(), "staging-app");
        // Hosts should still be present from base
        assert!(parsed.get("hosts").is_some());
    }

    #[test]
    fn test_load_and_merge_missing_overlay() {
        let dir = tempfile::TempDir::new().unwrap();
        let base_path = dir.path().join("korgi.toml");
        std::fs::write(
            &base_path,
            r#"
            [project]
            name = "app"
        "#,
        )
        .unwrap();

        let result = load_and_merge(&base_path, Some("production"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_load_and_merge_missing_base() {
        let dir = tempfile::TempDir::new().unwrap();
        let base_path = dir.path().join("nonexistent.toml");
        let result = load_and_merge(&base_path, None);
        assert!(result.is_err());
    }
}
