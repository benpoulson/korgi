use std::collections::HashMap;

/// Interpolate `${VAR}` patterns in a string using environment variables.
/// Returns the interpolated string or an error if a referenced variable is not set.
pub fn interpolate_str(input: &str, env: &HashMap<String, String>) -> anyhow::Result<String> {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

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
    fn test_empty_var_name() {
        let env = HashMap::new();
        assert!(interpolate_str("${}", &env).is_err());
    }
}
