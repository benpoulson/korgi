use bollard::auth::DockerCredentials;

use crate::config::types::RegistryConfig;

/// Registry host used for images without an explicit registry.
const DOCKER_HUB: &str = "docker.io";

/// Build Docker credentials for a registry, if configured.
///
/// A registry matches when its host (scheme, path and trailing slash removed,
/// case-insensitive) is exactly the registry host of the image reference.
pub fn credentials_for_image(
    image: &str,
    registries: &[RegistryConfig],
) -> Option<DockerCredentials> {
    let image_host = image_registry_host(image);
    for reg in registries {
        let url = reg.resolved_url();
        let Some(reg_host) = registry_url_host(url) else {
            continue;
        };
        if reg_host == image_host {
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

/// Normalised registry host of a configured registry URL, e.g.
/// `https://GHCR.io/` -> `ghcr.io`. `None` for an empty URL.
fn registry_url_host(url: &str) -> Option<String> {
    let url = url.trim();
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = without_scheme.split('/').next().unwrap_or("");
    // Drop any userinfo ("user@host").
    let host = host.rsplit('@').next().unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some(normalize_host(host))
}

/// Registry host of an image reference, following Docker's rules: the first
/// path component is a registry only if it contains `.` or `:` or is
/// `localhost`; otherwise the image lives on Docker Hub.
fn image_registry_host(image: &str) -> String {
    match image.split_once('/') {
        Some((first, _))
            if first.contains('.')
                || first.contains(':')
                || first.eq_ignore_ascii_case("localhost") =>
        {
            normalize_host(first)
        }
        _ => DOCKER_HUB.to_string(),
    }
}

fn normalize_host(host: &str) -> String {
    let host = host.to_ascii_lowercase();
    match host.as_str() {
        "index.docker.io" | "registry-1.docker.io" | "registry.hub.docker.com" => {
            DOCKER_HUB.to_string()
        }
        _ => host,
    }
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

    fn registry(url: &str) -> RegistryConfig {
        RegistryConfig {
            url: Some(url.to_string()),
            username: Some("u".to_string()),
            password: Some("p".to_string()),
            github_token: None,
        }
    }

    #[test]
    fn test_lookalike_host_does_not_get_credentials() {
        let regs = test_registries();
        assert!(credentials_for_image("ghcr.io.evil.com/myorg/app:latest", &regs).is_none());
        assert!(credentials_for_image("ghcr.iox/app:latest", &regs).is_none());
        assert!(credentials_for_image("evil.com/ghcr.io/app:latest", &regs).is_none());
    }

    #[test]
    fn test_scheme_and_trailing_slash_are_normalised() {
        for url in [
            "https://ghcr.io",
            "https://ghcr.io/",
            "http://ghcr.io/",
            "ghcr.io/",
            "GHCR.IO",
        ] {
            let creds = credentials_for_image("ghcr.io/org/app:v1", &[registry(url)]);
            assert!(creds.is_some(), "{} should match", url);
            assert_eq!(creds.unwrap().serveraddress, Some(url.to_string()));
        }
    }

    #[test]
    fn test_url_with_path_matches_on_host_only() {
        let regs = [registry("https://registry.example.com/v2/")];
        assert!(credentials_for_image("registry.example.com/app:v1", &regs).is_some());
        assert!(credentials_for_image("registry.example.com.evil/app:v1", &regs).is_none());
    }

    #[test]
    fn test_registry_with_port() {
        let regs = [registry("localhost:5000")];
        assert!(credentials_for_image("localhost:5000/app:v1", &regs).is_some());
        assert!(credentials_for_image("localhost:5001/app:v1", &regs).is_none());
        assert!(credentials_for_image("localhost/app:v1", &regs).is_none());
    }

    #[test]
    fn test_docker_hub_aliases() {
        let regs = [registry("https://index.docker.io/v1/")];
        assert!(credentials_for_image("nginx:latest", &regs).is_some());
        assert!(credentials_for_image("myorg/app:v1", &regs).is_some());
        assert!(credentials_for_image("docker.io/myorg/app:v1", &regs).is_some());
        assert!(credentials_for_image("ghcr.io/myorg/app:v1", &regs).is_none());
    }

    #[test]
    fn test_empty_url_never_matches() {
        assert!(credentials_for_image("nginx:latest", &[registry("")]).is_none());
        assert!(credentials_for_image("nginx:latest", &[registry("https://")]).is_none());
    }

    #[test]
    fn test_image_registry_host() {
        assert_eq!(image_registry_host("nginx"), "docker.io");
        assert_eq!(image_registry_host("library/nginx:1"), "docker.io");
        assert_eq!(image_registry_host("GHCR.io/a/b"), "ghcr.io");
        assert_eq!(image_registry_host("localhost/a"), "localhost");
        assert_eq!(image_registry_host("host:5000/a"), "host:5000");
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
