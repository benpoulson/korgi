use bollard::auth::DockerCredentials;

use crate::config::types::RegistryConfig;

/// Build Docker credentials for a registry, if configured.
pub fn credentials_for_image(
    image: &str,
    registries: &[RegistryConfig],
) -> Option<DockerCredentials> {
    // Match the image's registry prefix against configured registries
    for reg in registries {
        if image.starts_with(&reg.url) {
            return Some(DockerCredentials {
                username: reg.username.clone(),
                password: reg.password.clone(),
                serveraddress: Some(reg.url.clone()),
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
                url: "ghcr.io".to_string(),
                username: Some("user1".to_string()),
                password: Some("pass1".to_string()),
            },
            RegistryConfig {
                url: "registry.example.com".to_string(),
                username: Some("user2".to_string()),
                password: Some("pass2".to_string()),
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
            url: "ghcr.io".to_string(),
            username: None,
            password: None,
        }];
        let creds = credentials_for_image("ghcr.io/myapp:latest", &regs);
        assert!(creds.is_some());
        assert_eq!(creds.as_ref().unwrap().username, None);
    }
}
