use std::collections::HashMap;

/// Interpolate `${VAR}` patterns in a string using environment variables.
/// Returns the interpolated string or an error if a referenced variable is not set.
/// Skips TOML comment lines (lines where first non-whitespace is `#`).
pub fn interpolate_str(input: &str, env: &HashMap<String, String>) -> anyhow::Result<String> {
    let mut result = String::with_capacity(input.len());

    for line in input.split('\n') {
        if !result.is_empty() {
            result.push('\n');
        }

        // Skip interpolation for comment lines
        if line.trim_start().starts_with('#') {
            result.push_str(line);
            continue;
        }

        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '$' && chars.peek() == Some(&'{') {
                chars.next(); // consume '{'
                let mut var_name = String::new();
                let mut found_close = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        found_close = true;
                        break;
                    }
                    var_name.push(c);
                }
                if !found_close {
                    anyhow::bail!("unclosed variable reference: ${{{}", var_name);
                }
                if var_name.is_empty() {
                    anyhow::bail!("empty variable name in interpolation");
                }
                match env.get(&var_name) {
                    Some(val) => result.push_str(val),
                    None => anyhow::bail!("environment variable '{}' is not set", var_name),
                }
            } else {
                result.push(c);
            }
        }
    }

    Ok(result)
}

/// Interpolate all string values in an environment map.
pub fn interpolate_env(
    env_map: &HashMap<String, String>,
    system_env: &HashMap<String, String>,
) -> anyhow::Result<HashMap<String, String>> {
    let mut result = HashMap::new();
    for (key, value) in env_map {
        result.insert(key.clone(), interpolate_str(value, system_env)?);
    }
    Ok(result)
}

/// Build a map of current system environment variables.
pub fn system_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_interpolation() {
        let env = HashMap::new();
        assert_eq!(interpolate_str("hello world", &env).unwrap(), "hello world");
    }

    #[test]
    fn test_simple_interpolation() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        assert_eq!(interpolate_str("${FOO}", &env).unwrap(), "bar");
    }

    #[test]
    fn test_interpolation_in_context() {
        let mut env = HashMap::new();
        env.insert("HOST".to_string(), "localhost".to_string());
        env.insert("PORT".to_string(), "5432".to_string());
        assert_eq!(
            interpolate_str("postgres://${HOST}:${PORT}/db", &env).unwrap(),
            "postgres://localhost:5432/db"
        );
    }

    #[test]
    fn test_missing_variable() {
        let env = HashMap::new();
        assert!(interpolate_str("${MISSING}", &env).is_err());
    }

    #[test]
    fn test_unclosed_brace() {
        let env = HashMap::new();
        assert!(interpolate_str("${UNCLOSED", &env).is_err());
    }

    #[test]
    fn test_dollar_without_brace() {
        let env = HashMap::new();
        assert_eq!(interpolate_str("$FOO", &env).unwrap(), "$FOO");
    }

    #[test]
    fn test_comment_lines_skipped() {
        let env = HashMap::new();
        // ${UNSET} in a comment should not trigger an error
        let input = "# username = \"${UNSET}\"\nvalue = \"literal\"";
        let result = interpolate_str(input, &env).unwrap();
        assert!(result.contains("${UNSET}")); // preserved verbatim
        assert!(result.contains("value = \"literal\""));
    }

    #[test]
    fn test_indented_comment_skipped() {
        let env = HashMap::new();
        let input = "  # password = \"${SECRET}\"";
        assert!(interpolate_str(input, &env).is_ok());
    }

    #[test]
    fn test_empty_var_name() {
        let env = HashMap::new();
        assert!(interpolate_str("${}", &env).is_err());
    }

    #[test]
    fn test_multiple_vars_in_one_string() {
        let mut env = HashMap::new();
        env.insert("USER".to_string(), "admin".to_string());
        env.insert("PASS".to_string(), "secret".to_string());
        env.insert("HOST".to_string(), "db.example.com".to_string());
        assert_eq!(
            interpolate_str("${USER}:${PASS}@${HOST}", &env).unwrap(),
            "admin:secret@db.example.com"
        );
    }

    #[test]
    fn test_adjacent_vars() {
        let mut env = HashMap::new();
        env.insert("A".to_string(), "hello".to_string());
        env.insert("B".to_string(), "world".to_string());
        assert_eq!(interpolate_str("${A}${B}", &env).unwrap(), "helloworld");
    }

    #[test]
    fn test_var_with_special_chars_in_value() {
        let mut env = HashMap::new();
        env.insert(
            "URL".to_string(),
            "postgres://user:p@ss${word@host/db".to_string(),
        );
        assert_eq!(
            interpolate_str("${URL}", &env).unwrap(),
            "postgres://user:p@ss${word@host/db"
        );
    }

    #[test]
    fn test_no_vars_passthrough() {
        let env = HashMap::new();
        let input = "just plain text with $signs and {braces}";
        assert_eq!(interpolate_str(input, &env).unwrap(), input);
    }

    #[test]
    fn test_interpolate_env_map() {
        let mut env_map = HashMap::new();
        env_map.insert("DB".to_string(), "postgres://${HOST}/db".to_string());
        env_map.insert("REDIS".to_string(), "redis://localhost".to_string());

        let mut sys_env = HashMap::new();
        sys_env.insert("HOST".to_string(), "prod-db".to_string());

        let result = interpolate_env(&env_map, &sys_env).unwrap();
        assert_eq!(result.get("DB").unwrap(), "postgres://prod-db/db");
        assert_eq!(result.get("REDIS").unwrap(), "redis://localhost");
    }

    #[test]
    fn test_interpolate_env_map_missing_var() {
        let mut env_map = HashMap::new();
        env_map.insert("DB".to_string(), "${MISSING_VAR}".to_string());
        let sys_env = HashMap::new();
        assert!(interpolate_env(&env_map, &sys_env).is_err());
    }

    #[test]
    fn test_system_env_returns_something() {
        let env = system_env();
        // PATH should always exist
        assert!(env.contains_key("PATH") || env.contains_key("HOME"));
    }
}
