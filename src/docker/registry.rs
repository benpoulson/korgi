use bollard::auth::DockerCredentials;

use crate::config::types::RegistryConfig;

/// Build Docker credentials for a registry, if configured.
pub fn credentials_for_image(
    image: &str,
    registries: &[RegistryConfig],
) -> Option<DockerCredentials> {
    for reg in registries {
        let url = reg.resolved_url();
        if !url.is_empty() && image.starts_with(url) {
            return Some(DockerCredentials {
                username: reg.resolved_username().map(|s| s.to_string()),
                password: reg.resolved_password().map(|s| s.to_string()),
                serveraddress: Some(url.to_string()),
                ..Default::default()
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_registries() -> Vec<RegistryConfig> {
        vec![
            RegistryConfig {
                url: Some("ghcr.io".to_string()),
                username: Some("user1".to_string()),
                password: Some("pass1".to_string()),
                github_token: None,
            },
            RegistryConfig {
                url: Some("registry.example.com".to_string()),
                username: Some("user2".to_string()),
                password: Some("pass2".to_string()),
                github_token: None,
            },
        ]
    }

    #[test]
    fn test_matches_ghcr() {
        let regs = test_registries();
        let creds = credentials_for_image("ghcr.io/myorg/myapp:latest", &regs);
        assert!(creds.is_some());
        let creds = creds.unwrap();
        assert_eq!(creds.username, Some("user1".to_string()));
        assert_eq!(creds.password, Some("pass1".to_string()));
        assert_eq!(creds.serveraddress, Some("ghcr.io".to_string()));
    }

    #[test]
    fn test_matches_custom_registry() {
        let regs = test_registries();
        let creds = credentials_for_image("registry.example.com/app:v1", &regs);
        assert!(creds.is_some());
        assert_eq!(creds.unwrap().username, Some("user2".to_string()));
    }

    #[test]
    fn test_no_match_dockerhub() {
        let regs = test_registries();
        let creds = credentials_for_image("nginx:latest", &regs);
        assert!(creds.is_none());
    }

    #[test]
    fn test_no_match_different_registry() {
        let regs = test_registries();
        let creds = credentials_for_image("quay.io/myapp:latest", &regs);
        assert!(creds.is_none());
    }

    #[test]
    fn test_empty_registries() {
        let creds = credentials_for_image("ghcr.io/myapp:latest", &[]);
        assert!(creds.is_none());
    }

    #[test]
    fn test_registry_without_credentials() {
        let regs = vec![RegistryConfig {
            url: Some("ghcr.io".to_string()),
            username: None,
            password: None,
            github_token: None,
        }];
        let creds = credentials_for_image("ghcr.io/myapp:latest", &regs);
        assert!(creds.is_some());
        assert_eq!(creds.as_ref().unwrap().username, None);
    }

    #[test]
    fn test_github_token_shorthand() {
        let regs = vec![RegistryConfig {
            url: None,
            username: None,
            password: None,
            github_token: Some("ghp_abc123".to_string()),
        }];
        let creds = credentials_for_image("ghcr.io/myorg/myapp:latest", &regs);
        assert!(creds.is_some());
        let creds = creds.unwrap();
        assert_eq!(creds.username, Some("token".to_string()));
        assert_eq!(creds.password, Some("ghp_abc123".to_string()));
        assert_eq!(creds.serveraddress, Some("ghcr.io".to_string()));
    }

    #[test]
    fn test_github_token_no_match_non_ghcr() {
        let regs = vec![RegistryConfig {
            url: None,
            username: None,
            password: None,
            github_token: Some("ghp_abc123".to_string()),
        }];
        let creds = credentials_for_image("docker.io/nginx:latest", &regs);
        assert!(creds.is_none());
    }
}
