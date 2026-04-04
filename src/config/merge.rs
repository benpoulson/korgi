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
        anyhow::anyhow!(
            "Failed to read config file {}: {}",
            base_path.display(),
            e
        )
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

    let mut base_val: Value = base_content.parse().map_err(|e| {
        anyhow::anyhow!("Failed to parse base config: {}", e)
    })?;
    let overlay_val: Value = overlay_content.parse().map_err(|e| {
        anyhow::anyhow!("Failed to parse overlay config: {}", e)
    })?;

    deep_merge(&mut base_val, overlay_val);

    Ok(toml::to_string(&base_val)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deep_merge_simple() {
        let mut base: Value = toml::from_str(r#"
            [project]
            name = "myapp"
            [traefik]
            image = "traefik:v3.1"
        "#).unwrap();

        let overlay: Value = toml::from_str(r#"
            [traefik]
            image = "traefik:v3.2"
        "#).unwrap();

        deep_merge(&mut base, overlay);

        assert_eq!(
            base["project"]["name"].as_str().unwrap(),
            "myapp"
        );
        assert_eq!(
            base["traefik"]["image"].as_str().unwrap(),
            "traefik:v3.2"
        );
    }

    #[test]
    fn test_deep_merge_nested() {
        let mut base: Value = toml::from_str(r#"
            [project]
            name = "myapp"
            [[services]]
            name = "api"
            image = "api:v1"
        "#).unwrap();

        let overlay: Value = toml::from_str(r#"
            [[services]]
            name = "api"
            image = "api:v2"
        "#).unwrap();

        deep_merge(&mut base, overlay);

        // Arrays are replaced, not merged
        let services = base["services"].as_array().unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0]["image"].as_str().unwrap(), "api:v2");
    }

    #[test]
    fn test_deep_merge_add_new_key() {
        let mut base: Value = toml::from_str(r#"
            [project]
            name = "myapp"
        "#).unwrap();

        let overlay: Value = toml::from_str(r#"
            [project]
            name = "myapp"
            [traefik]
            image = "traefik:v3.2"
        "#).unwrap();

        deep_merge(&mut base, overlay);

        assert_eq!(
            base["traefik"]["image"].as_str().unwrap(),
            "traefik:v3.2"
        );
    }
}
