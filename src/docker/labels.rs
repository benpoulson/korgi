use std::collections::HashMap;

use crate::config::types::ServiceConfig;

const LABEL_PREFIX: &str = "korgi";

/// Generate korgi metadata labels for a container.
pub fn metadata_labels(
    project: &str,
    service: &str,
    generation: u64,
    instance: u32,
    image: &str,
) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    labels.insert(format!("{}.project", LABEL_PREFIX), project.to_string());
    labels.insert(format!("{}.service", LABEL_PREFIX), service.to_string());
    labels.insert(
        format!("{}.generation", LABEL_PREFIX),
        generation.to_string(),
    );
    labels.insert(format!("{}.instance", LABEL_PREFIX), instance.to_string());
    labels.insert(format!("{}.image", LABEL_PREFIX), image.to_string());
    labels
}

/// Generate Traefik routing labels for a container.
pub fn traefik_labels(
    project: &str,
    service: &str,
    svc_config: &ServiceConfig,
    network: &str,
) -> HashMap<String, String> {
    let mut labels = HashMap::new();

    let Some(routing) = &svc_config.routing else {
        return labels;
    };

    let router_name = format!("{}-{}", project, service);

    labels.insert("traefik.enable".to_string(), "true".to_string());
    labels.insert(
        format!("traefik.http.routers.{}.rule", router_name),
        routing.rule.clone(),
    );

    if !routing.entrypoints.is_empty() {
        labels.insert(
            format!("traefik.http.routers.{}.entrypoints", router_name),
            routing.entrypoints.join(","),
        );
    }

    if routing.tls {
        labels.insert(
            format!("traefik.http.routers.{}.tls", router_name),
            "true".to_string(),
        );
        labels.insert(
            format!("traefik.http.routers.{}.tls.certresolver", router_name),
            "letsencrypt".to_string(),
        );
    }

    // Set the service port if configured
    if let Some(ports) = &svc_config.ports {
        labels.insert(
            format!(
                "traefik.http.services.{}.loadbalancer.server.port",
                router_name
            ),
            ports.container.to_string(),
        );

        // Add Traefik-side health check labels if the service has health config
        if let Some(health) = &svc_config.health {
            labels.insert(
                format!(
                    "traefik.http.services.{}.loadbalancer.healthcheck.path",
                    router_name
                ),
                health.path.clone(),
            );
            labels.insert(
                format!(
                    "traefik.http.services.{}.loadbalancer.healthcheck.interval",
                    router_name
                ),
                health.interval.clone(),
            );
            labels.insert(
                format!(
                    "traefik.http.services.{}.loadbalancer.healthcheck.timeout",
                    router_name
                ),
                health.timeout.clone(),
            );
            labels.insert(
                format!(
                    "traefik.http.services.{}.loadbalancer.healthcheck.port",
                    router_name
                ),
                ports.container.to_string(),
            );
        }
    }

    // Set the Docker network for Traefik to use
    labels.insert("traefik.docker.network".to_string(), network.to_string());

    labels
}

/// Generate all labels for a container (metadata + traefik).
pub fn all_labels(
    project: &str,
    svc_config: &ServiceConfig,
    generation: u64,
    instance: u32,
    traefik_network: &str,
) -> HashMap<String, String> {
    let mut labels = metadata_labels(
        project,
        &svc_config.name,
        generation,
        instance,
        &svc_config.image,
    );

    let traefik = traefik_labels(project, &svc_config.name, svc_config, traefik_network);
    labels.extend(traefik);

    labels
}

/// Build a label filter to find containers belonging to a project.
pub fn project_filter(project: &str) -> HashMap<String, Vec<String>> {
    let mut filters = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![format!("{}.project={}", LABEL_PREFIX, project)],
    );
    filters
}

/// Build a label filter to find containers belonging to a specific service.
pub fn service_filter(project: &str, service: &str) -> HashMap<String, Vec<String>> {
    let mut filters = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![
            format!("{}.project={}", LABEL_PREFIX, project),
            format!("{}.service={}", LABEL_PREFIX, service),
        ],
    );
    filters
}

/// Build a label filter for a specific generation.
pub fn generation_filter(
    project: &str,
    service: &str,
    generation: u64,
) -> HashMap<String, Vec<String>> {
    let mut filters = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![
            format!("{}.project={}", LABEL_PREFIX, project),
            format!("{}.service={}", LABEL_PREFIX, service),
            format!("{}.generation={}", LABEL_PREFIX, generation),
        ],
    );
    filters
}

/// Container name following the korgi convention.
pub fn container_name(project: &str, service: &str, generation: u64, instance: u32) -> String {
    format!("korgi-{}-{}-g{}-{}", project, service, generation, instance)
}

/// Parse generation number from a container's labels.
pub fn parse_generation(labels: &HashMap<String, String>) -> Option<u64> {
    labels
        .get(&format!("{}.generation", LABEL_PREFIX))
        .and_then(|v| v.parse().ok())
}

/// Parse instance number from a container's labels.
pub fn parse_instance(labels: &HashMap<String, String>) -> Option<u32> {
    labels
        .get(&format!("{}.instance", LABEL_PREFIX))
        .and_then(|v| v.parse().ok())
}

/// Parse service name from a container's labels.
pub fn parse_service(labels: &HashMap<String, String>) -> Option<String> {
    labels.get(&format!("{}.service", LABEL_PREFIX)).cloned()
}

/// Parse image from a container's labels.
pub fn parse_image(labels: &HashMap<String, String>) -> Option<String> {
    labels.get(&format!("{}.image", LABEL_PREFIX)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{PortsConfig, RoutingConfig};

    #[test]
    fn test_container_name() {
        assert_eq!(container_name("myapp", "api", 4, 0), "korgi-myapp-api-g4-0");
    }

    #[test]
    fn test_metadata_labels() {
        let labels = metadata_labels("myapp", "api", 4, 1, "myapp/api:v2");
        assert_eq!(labels.get("korgi.project").unwrap(), "myapp");
        assert_eq!(labels.get("korgi.service").unwrap(), "api");
        assert_eq!(labels.get("korgi.generation").unwrap(), "4");
        assert_eq!(labels.get("korgi.instance").unwrap(), "1");
        assert_eq!(labels.get("korgi.image").unwrap(), "myapp/api:v2");
    }

    #[test]
    fn test_traefik_labels_with_routing() {
        let svc = ServiceConfig {
            name: "api".to_string(),
            image: "api:latest".to_string(),
            replicas: 1,
            placement_labels: vec![],
            command: None,
            entrypoint: None,
            restart: "unless-stopped".to_string(),
            health: None,
            routing: Some(RoutingConfig {
                rule: "Host(`api.example.com`)".to_string(),
                entrypoints: vec!["websecure".to_string()],
                tls: true,
            }),
            env: HashMap::new(),
            ports: Some(PortsConfig {
                container: 8080,
                host: None,
                host_base: None,
            }),
            volumes: vec![],
            resources: None,
            deploy: None,
        };

        let labels = traefik_labels("myapp", "api", &svc, "korgi-traefik");
        assert_eq!(labels.get("traefik.enable").unwrap(), "true");
        assert_eq!(
            labels.get("traefik.http.routers.myapp-api.rule").unwrap(),
            "Host(`api.example.com`)"
        );
        assert_eq!(
            labels
                .get("traefik.http.routers.myapp-api.entrypoints")
                .unwrap(),
            "websecure"
        );
        assert_eq!(
            labels.get("traefik.http.routers.myapp-api.tls").unwrap(),
            "true"
        );
        assert_eq!(
            labels
                .get("traefik.http.services.myapp-api.loadbalancer.server.port")
                .unwrap(),
            "8080"
        );
    }

    #[test]
    fn test_traefik_labels_without_routing() {
        let svc = ServiceConfig {
            name: "worker".to_string(),
            image: "worker:latest".to_string(),
            replicas: 1,
            placement_labels: vec![],
            command: None,
            entrypoint: None,
            restart: "unless-stopped".to_string(),
            health: None,
            routing: None,
            env: HashMap::new(),
            ports: None,
            volumes: vec![],
            resources: None,
            deploy: None,
        };

        let labels = traefik_labels("myapp", "worker", &svc, "korgi-traefik");
        assert!(labels.is_empty());
    }

    #[test]
    fn test_parse_generation() {
        let mut labels = HashMap::new();
        labels.insert("korgi.generation".to_string(), "42".to_string());
        assert_eq!(parse_generation(&labels), Some(42));
    }

    #[test]
    fn test_parse_generation_missing() {
        let labels = HashMap::new();
        assert_eq!(parse_generation(&labels), None);
    }

    #[test]
    fn test_parse_generation_invalid() {
        let mut labels = HashMap::new();
        labels.insert("korgi.generation".to_string(), "notanumber".to_string());
        assert_eq!(parse_generation(&labels), None);
    }

    #[test]
    fn test_parse_instance() {
        let mut labels = HashMap::new();
        labels.insert("korgi.instance".to_string(), "5".to_string());
        assert_eq!(parse_instance(&labels), Some(5));
    }

    #[test]
    fn test_parse_instance_missing() {
        let labels = HashMap::new();
        assert_eq!(parse_instance(&labels), None);
    }

    #[test]
    fn test_parse_service() {
        let mut labels = HashMap::new();
        labels.insert("korgi.service".to_string(), "api".to_string());
        assert_eq!(parse_service(&labels), Some("api".to_string()));
    }

    #[test]
    fn test_parse_service_missing() {
        let labels = HashMap::new();
        assert_eq!(parse_service(&labels), None);
    }

    #[test]
    fn test_parse_image() {
        let mut labels = HashMap::new();
        labels.insert("korgi.image".to_string(), "myapp/api:v2".to_string());
        assert_eq!(parse_image(&labels), Some("myapp/api:v2".to_string()));
    }

    #[test]
    fn test_project_filter() {
        let filters = project_filter("myapp");
        let labels = filters.get("label").unwrap();
        assert_eq!(labels.len(), 1);
        assert!(labels.contains(&"korgi.project=myapp".to_string()));
    }

    #[test]
    fn test_service_filter() {
        let filters = service_filter("myapp", "api");
        let labels = filters.get("label").unwrap();
        assert_eq!(labels.len(), 2);
        assert!(labels.contains(&"korgi.project=myapp".to_string()));
        assert!(labels.contains(&"korgi.service=api".to_string()));
    }

    #[test]
    fn test_generation_filter() {
        let filters = generation_filter("myapp", "api", 5);
        let labels = filters.get("label").unwrap();
        assert_eq!(labels.len(), 3);
        assert!(labels.contains(&"korgi.project=myapp".to_string()));
        assert!(labels.contains(&"korgi.service=api".to_string()));
        assert!(labels.contains(&"korgi.generation=5".to_string()));
    }

    #[test]
    fn test_container_name_various() {
        assert_eq!(container_name("app", "web", 1, 0), "korgi-app-web-g1-0");
        assert_eq!(
            container_name("myapp", "worker", 10, 3),
            "korgi-myapp-worker-g10-3"
        );
    }

    #[test]
    fn test_all_labels_with_routing() {
        let svc = ServiceConfig {
            name: "api".to_string(),
            image: "myapp/api:v3".to_string(),
            replicas: 1,
            placement_labels: vec![],
            command: None,
            entrypoint: None,
            restart: "unless-stopped".to_string(),
            health: None,
            routing: Some(RoutingConfig {
                rule: "Host(`api.example.com`)".to_string(),
                entrypoints: vec!["web".to_string()],
                tls: false,
            }),
            env: HashMap::new(),
            ports: Some(PortsConfig {
                container: 3000,
                host: None,
                host_base: None,
            }),
            volumes: vec![],
            resources: None,
            deploy: None,
        };

        let labels = all_labels("proj", &svc, 7, 2, "my-net");
        // Should have both metadata and traefik labels
        assert_eq!(labels.get("korgi.project").unwrap(), "proj");
        assert_eq!(labels.get("korgi.service").unwrap(), "api");
        assert_eq!(labels.get("korgi.generation").unwrap(), "7");
        assert_eq!(labels.get("korgi.instance").unwrap(), "2");
        assert_eq!(labels.get("korgi.image").unwrap(), "myapp/api:v3");
        assert_eq!(labels.get("traefik.enable").unwrap(), "true");
        assert_eq!(
            labels
                .get("traefik.http.services.proj-api.loadbalancer.server.port")
                .unwrap(),
            "3000"
        );
        assert_eq!(labels.get("traefik.docker.network").unwrap(), "my-net");
    }

    #[test]
    fn test_all_labels_without_routing() {
        let svc = ServiceConfig {
            name: "worker".to_string(),
            image: "worker:latest".to_string(),
            replicas: 1,
            placement_labels: vec![],
            command: None,
            entrypoint: None,
            restart: "unless-stopped".to_string(),
            health: None,
            routing: None,
            env: HashMap::new(),
            ports: None,
            volumes: vec![],
            resources: None,
            deploy: None,
        };

        let labels = all_labels("proj", &svc, 1, 0, "net");
        // Should have metadata but no traefik labels
        assert_eq!(labels.get("korgi.project").unwrap(), "proj");
        assert!(labels.get("traefik.enable").is_none());
        // Exactly 5 metadata labels
        assert_eq!(labels.len(), 5);
    }

    #[test]
    fn test_traefik_labels_tls_certresolver() {
        let svc = ServiceConfig {
            name: "api".to_string(),
            image: "api:latest".to_string(),
            replicas: 1,
            placement_labels: vec![],
            command: None,
            entrypoint: None,
            restart: "unless-stopped".to_string(),
            health: None,
            routing: Some(RoutingConfig {
                rule: "Host(`secure.example.com`)".to_string(),
                entrypoints: vec!["websecure".to_string()],
                tls: true,
            }),
            env: HashMap::new(),
            ports: Some(PortsConfig {
                container: 443,
                host: None,
                host_base: None,
            }),
            volumes: vec![],
            resources: None,
            deploy: None,
        };

        let labels = traefik_labels("proj", "api", &svc, "net");
        assert_eq!(
            labels.get("traefik.http.routers.proj-api.tls").unwrap(),
            "true"
        );
        assert_eq!(
            labels
                .get("traefik.http.routers.proj-api.tls.certresolver")
                .unwrap(),
            "letsencrypt"
        );
    }

    #[test]
    fn test_traefik_labels_multiple_entrypoints() {
        let svc = ServiceConfig {
            name: "api".to_string(),
            image: "api:latest".to_string(),
            replicas: 1,
            placement_labels: vec![],
            command: None,
            entrypoint: None,
            restart: "unless-stopped".to_string(),
            health: None,
            routing: Some(RoutingConfig {
                rule: "Host(`example.com`)".to_string(),
                entrypoints: vec!["web".to_string(), "websecure".to_string()],
                tls: false,
            }),
            env: HashMap::new(),
            ports: None,
            volumes: vec![],
            resources: None,
            deploy: None,
        };

        let labels = traefik_labels("proj", "api", &svc, "net");
        assert_eq!(
            labels
                .get("traefik.http.routers.proj-api.entrypoints")
                .unwrap(),
            "web,websecure"
        );
    }

    #[test]
    fn test_traefik_labels_no_port_no_port_label() {
        let svc = ServiceConfig {
            name: "api".to_string(),
            image: "api:latest".to_string(),
            replicas: 1,
            placement_labels: vec![],
            command: None,
            entrypoint: None,
            restart: "unless-stopped".to_string(),
            health: None,
            routing: Some(RoutingConfig {
                rule: "Host(`example.com`)".to_string(),
                entrypoints: vec![],
                tls: false,
            }),
            env: HashMap::new(),
            ports: None,
            volumes: vec![],
            resources: None,
            deploy: None,
        };

        let labels = traefik_labels("proj", "api", &svc, "net");
        assert!(
            labels
                .get("traefik.http.services.proj-api.loadbalancer.server.port")
                .is_none()
        );
    }

    #[test]
    fn test_traefik_labels_with_health_check() {
        use crate::config::types::HealthConfig;

        let svc = ServiceConfig {
            name: "api".to_string(),
            image: "api:latest".to_string(),
            replicas: 1,
            placement_labels: vec![],
            command: None,
            entrypoint: None,
            restart: "unless-stopped".to_string(),
            health: Some(HealthConfig {
                mode: Default::default(),
                path: "/ready".to_string(),
                interval: "10s".to_string(),
                timeout: "5s".to_string(),
                retries: 3,
                start_period: None,
            }),
            routing: Some(RoutingConfig {
                rule: "Host(`api.example.com`)".to_string(),
                entrypoints: vec!["web".to_string()],
                tls: false,
            }),
            env: HashMap::new(),
            ports: Some(PortsConfig {
                container: 8080,
                host: None,
                host_base: None,
            }),
            volumes: vec![],
            resources: None,
            deploy: None,
        };

        let labels = traefik_labels("proj", "api", &svc, "net");
        assert_eq!(
            labels
                .get("traefik.http.services.proj-api.loadbalancer.healthcheck.path")
                .unwrap(),
            "/ready"
        );
        assert_eq!(
            labels
                .get("traefik.http.services.proj-api.loadbalancer.healthcheck.interval")
                .unwrap(),
            "10s"
        );
        assert_eq!(
            labels
                .get("traefik.http.services.proj-api.loadbalancer.healthcheck.timeout")
                .unwrap(),
            "5s"
        );
        assert_eq!(
            labels
                .get("traefik.http.services.proj-api.loadbalancer.healthcheck.port")
                .unwrap(),
            "8080"
        );
    }

    #[test]
    fn test_traefik_labels_no_health_check_no_healthcheck_labels() {
        let svc = ServiceConfig {
            name: "api".to_string(),
            image: "api:latest".to_string(),
            replicas: 1,
            placement_labels: vec![],
            command: None,
            entrypoint: None,
            restart: "unless-stopped".to_string(),
            health: None, // no health check
            routing: Some(RoutingConfig {
                rule: "Host(`api.example.com`)".to_string(),
                entrypoints: vec!["web".to_string()],
                tls: false,
            }),
            env: HashMap::new(),
            ports: Some(PortsConfig {
                container: 8080,
                host: None,
                host_base: None,
            }),
            volumes: vec![],
            resources: None,
            deploy: None,
        };

        let labels = traefik_labels("proj", "api", &svc, "net");
        // Should have port but no health check labels
        assert!(
            labels
                .get("traefik.http.services.proj-api.loadbalancer.server.port")
                .is_some()
        );
        assert!(
            labels
                .get("traefik.http.services.proj-api.loadbalancer.healthcheck.path")
                .is_none()
        );
    }
}
