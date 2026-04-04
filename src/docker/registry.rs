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
