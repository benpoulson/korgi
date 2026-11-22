//! Generates Traefik dynamic configuration (file provider) for cross-host load balancing.
//!
//! When containers are spread across multiple hosts, Traefik's Docker provider
//! only sees local containers. This module generates a YAML config file listing
//! all backends by internal_ip:host_port so Traefik can route to any host.

use std::collections::HashMap;
use tracing::warn;

use crate::config::types::{Config, ServiceConfig};
use crate::docker::containers::KorgiContainer;
use crate::orchestrator::state::LiveState;

/// Quote a string as a YAML double-quoted scalar.
///
/// JSON string syntax is a subset of YAML's double-quoted style, so serde_json's
/// escaping (quotes, backslashes, control characters, line breaks) produces a
/// scalar that YAML parses back to exactly the original text.
pub fn yaml_str(s: &str) -> String {
    serde_json::to_string(s).expect("serializing a string cannot fail")
}

/// Generate Traefik dynamic YAML config for all running services.
/// Maps each service to its backends across hosts using internal addresses and host ports.
pub fn generate_dynamic_config(config: &Config, state: &LiveState) -> String {
    let mut routers = Vec::new();
    let mut services = Vec::new();

    for svc_config in &config.services {
        let Some(routing) = &svc_config.routing else {
            continue; // No routing = no Traefik config (background worker)
        };
        if svc_config.ports.is_none() {
            continue; // No ports = can't route
        }

        let running = state.running_service_containers(&svc_config.name);
        if running.is_empty() {
            continue;
        }

        let router_name = yaml_str(&format!("{}-{}", config.project.name, svc_config.name));
        let servers = build_server_list(config, svc_config, &running);

        if servers.is_empty() {
            continue;
        }

        // Router
        let mut router_yaml = format!(
            "    {}:\n      rule: {}\n      service: {}",
            router_name,
            yaml_str(&routing.rule),
            router_name,
        );

        if !routing.entrypoints.is_empty() {
            router_yaml.push_str(&format!(
                "\n      entryPoints:\n{}",
                routing
                    .entrypoints
                    .iter()
                    .map(|ep| format!("        - {}", yaml_str(ep)))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        if routing.tls {
            router_yaml.push_str("\n      tls:\n        certResolver: letsencrypt");
        }

        routers.push(router_yaml);

        // Service with load balancer
        let mut service_yaml = format!(
            "    {}:\n      loadBalancer:\n        servers:\n{}",
            router_name,
            servers
                .iter()
                .map(|url| format!("          - url: {}", yaml_str(url)))
                .collect::<Vec<_>>()
                .join("\n")
        );

        // Load balancing strategy
        if let Some(strategy) = routing.lb_strategy.as_ref() {
            let traefik_strategy = match strategy.as_str() {
                "roundrobin" => "wrr",
                "leastconnections" => "p2c",
                other => other,
            };
            service_yaml.push_str(&format!(
                "\n        strategy: {}",
                yaml_str(traefik_strategy)
            ));
        }

        // Sticky sessions
        if let Some(sticky) = &routing.sticky {
            let cookie_name = sticky
                .cookie_name
                .clone()
                .unwrap_or_else(|| format!("korgi_{}", svc_config.name));
            service_yaml.push_str(&format!(
                "\n        sticky:\n          cookie:\n            name: {}\n            secure: {}\n            httpOnly: {}",
                yaml_str(&cookie_name),
                sticky.secure,
                sticky.http_only
            ));
        }

        // Add health check if configured with an HTTP path
        if let Some(health) = svc_config.health.as_ref().filter(|h| !h.path.is_empty()) {
            service_yaml.push_str(&format!(
                "\n        healthCheck:\n          path: {}\n          interval: {}\n          timeout: {}",
                yaml_str(&health.path),
                yaml_str(&health.interval),
                yaml_str(&health.timeout)
            ));
            // Without a hostname Traefik probes with the backend IP as the Host
            // header; apps with a host allow-list answer 400 and Traefik then
            // marks every backend down. Probe with the routed hostname instead.
            if let Some(host) = first_host_in_rule(&routing.rule) {
                service_yaml.push_str(&format!("\n          hostname: {}", yaml_str(&host)));
            }
        }

        services.push(service_yaml);
    }

    if routers.is_empty() && services.is_empty() {
        return "# No routable services\nhttp: {}\n".to_string();
    }

    let mut yaml = "# Generated by korgi -- do not edit manually\nhttp:\n".to_string();

    if !routers.is_empty() {
        yaml.push_str("  routers:\n");
        yaml.push_str(&routers.join("\n"));
        yaml.push('\n');
    }

    if !services.is_empty() {
        yaml.push_str("  services:\n");
        yaml.push_str(&services.join("\n"));
        yaml.push('\n');
    }

    yaml
}

/// Build the list of backend server URLs for a service.
/// Each running container gets an entry: http://{internal_ip}:{host_port}, where
/// host_port is the port the container actually published (recorded at creation).
fn build_server_list(
    config: &Config,
    svc: &ServiceConfig,
    running: &[&KorgiContainer],
) -> Vec<String> {
    let host_map: HashMap<&str, &crate::config::types::HostConfig> =
        config.hosts.iter().map(|h| (h.name.as_str(), h)).collect();

    let ports = svc.ports.as_ref();
    let container_port = ports.map(|p| p.container).unwrap_or(80);
    let expects_host_port = ports.is_some_and(|p| p.host_base.is_some() || p.host.is_some());

    let mut urls = Vec::new();

    for container in running {
        let Some(host_cfg) = host_map.get(container.host_name.as_str()) else {
            continue;
        };

        let internal_ip = host_cfg.internal_addr();

        let port = match container.host_port {
            Some(port) => port,
            None if expects_host_port => {
                // Never guess: a recomputed port may belong to another container.
                warn!(
                    "Container {} on {} has no published host port; not routing to it",
                    container.name, container.host_name
                );
                continue;
            }
            // No host port configured -- use container port (assumes shared network)
            None => container_port,
        };

        urls.push(format!("http://{}:{}", internal_ip, port));
    }

    urls
}

/// First hostname matched by a `Host(...)` matcher in a Traefik rule, e.g.
/// ``Host(`api.example.com`) && PathPrefix(`/v1`)`` → `api.example.com`.
/// `HostRegexp` and `HostSNI` are not hostnames and are ignored.
fn first_host_in_rule(rule: &str) -> Option<String> {
    let mut rest = rule;
    while let Some(idx) = rest.find("Host(") {
        let preceded_by_ident = rest[..idx]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let args = &rest[idx + "Host(".len()..];
        if !preceded_by_ident {
            let args = args.trim_start();
            if let Some(quoted) = args.strip_prefix('`')
                && let Some(end) = quoted.find('`')
            {
                let host = quoted[..end].trim();
                if !host.is_empty() {
                    return Some(host.to_string());
                }
            }
        }
        rest = args;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::*;
    use crate::docker::containers::KorgiContainer;

    fn test_config() -> Config {
        Config {
            project: ProjectConfig {
                name: "myapp".to_string(),
                secrets: None,
            },
            registries: vec![],
            hosts: vec![
                {
                    let mut h = HostConfig::test_host("web1", "203.0.113.1");
                    h.internal_address = Some("10.0.0.1".to_string());
                    h.labels = vec!["web".to_string()];
                    h
                },
                {
                    let mut h = HostConfig::test_host("web2", "203.0.113.2");
                    h.internal_address = Some("10.0.0.2".to_string());
                    h.labels = vec!["web".to_string()];
                    h
                },
            ],
            traefik: Some(TraefikConfig {
                image: "traefik:v3.2".to_string(),
                hosts: vec!["web1".to_string()],
                entrypoints: HashMap::new(),
                network: "korgi-traefik".to_string(),
                acme: None,
            }),
            services: vec![{
                let mut svc = ServiceConfig::test_service("api", "myapp/api:v1");
                svc.replicas = 3;
                svc.placement_labels = vec!["web".to_string()];
                svc.routing = Some(RoutingConfig {
                    lb_strategy: None,
                    sticky: None,
                    rule: "Host(`api.example.com`)".to_string(),
                    entrypoints: vec!["websecure".to_string()],
                    tls: true,
                });
                svc.ports = Some(PortsConfig {
                    container: 8080,
                    host: None,
                    host_base: Some(9001),
                });
                svc.health = Some(HealthConfig {
                    mode: Default::default(),
                    path: "/health".to_string(),
                    interval: "10s".to_string(),
                    timeout: "3s".to_string(),
                    retries: 3,
                    start_period: None,
                });
                svc
            }],
        }
    }

    fn make_container(service: &str, generation: u64, instance: u32, host: &str) -> KorgiContainer {
        KorgiContainer {
            id: format!("{}-g{}-{}", service, generation, instance),
            name: format!("korgi-myapp-{}-g{}-{}", service, generation, instance),
            host_name: host.to_string(),
            service: service.to_string(),
            generation,
            instance,
            image: format!("myapp/{}:v1", service),
            state: "running".to_string(),
            status: "Up 5 minutes".to_string(),
            health: Some("healthy".to_string()),
            host_port: Some(9001 + instance as u16),
            host_port_known: true,
        }
    }

    fn with_port(mut c: KorgiContainer, port: u16) -> KorgiContainer {
        c.host_port = Some(port);
        c
    }

    #[test]
    fn test_generate_dynamic_config_basic() {
        let config = test_config();
        let state = LiveState {
            containers: vec![
                with_port(make_container("api", 3, 0, "web1"), 9007),
                with_port(make_container("api", 3, 1, "web2"), 9008),
                with_port(make_container("api", 3, 2, "web1"), 9009),
            ],
        };

        let yaml = generate_dynamic_config(&config, &state);

        assert!(yaml.contains("Generated by korgi"));
        assert!(yaml.contains("myapp-api"));
        assert!(yaml.contains("Host(`api.example.com`)"));
        // Ports come from each container's recorded host port
        assert!(yaml.contains("http://10.0.0.1:9007")); // instance 0 on web1
        assert!(yaml.contains("http://10.0.0.2:9008")); // instance 1 on web2
        assert!(yaml.contains("http://10.0.0.1:9009")); // instance 2 on web1
        assert!(yaml.contains("healthCheck"));
        assert!(yaml.contains("/health"));
        // Traefik probes with the routed hostname, not the backend IP
        assert!(yaml.contains("hostname: \"api.example.com\""), "{yaml}");
        assert!(yaml.contains("certResolver: letsencrypt"));
    }

    #[test]
    fn test_generate_config_uses_internal_address() {
        let config = test_config();
        let state = LiveState {
            containers: vec![make_container("api", 1, 0, "web1")],
        };

        let yaml = generate_dynamic_config(&config, &state);

        // Should use internal IP, not public
        assert!(yaml.contains("10.0.0.1"));
        assert!(!yaml.contains("203.0.113.1"));
    }

    #[test]
    fn test_generate_config_no_internal_falls_back() {
        let mut config = test_config();
        config.hosts[0].internal_address = None; // no internal address
        let state = LiveState {
            containers: vec![make_container("api", 1, 0, "web1")],
        };

        let yaml = generate_dynamic_config(&config, &state);

        // Should fall back to public address
        assert!(yaml.contains("203.0.113.1"));
    }

    #[test]
    fn test_generate_config_no_running_containers() {
        let config = test_config();
        let state = LiveState { containers: vec![] };

        let yaml = generate_dynamic_config(&config, &state);
        assert!(yaml.contains("No routable services"));
    }

    #[test]
    fn test_generate_config_worker_excluded() {
        let mut config = test_config();
        config.services.push({
            let mut svc = ServiceConfig::test_service("worker", "myapp/worker:v1");
            svc.replicas = 1;
            // No routing = not in Traefik config
            svc
        });

        let state = LiveState {
            containers: vec![
                make_container("api", 1, 0, "web1"),
                make_container("worker", 1, 0, "web1"),
            ],
        };

        let yaml = generate_dynamic_config(&config, &state);

        assert!(yaml.contains("myapp-api"));
        assert!(!yaml.contains("worker"));
    }

    #[test]
    fn test_generate_config_multiple_entrypoints() {
        let mut config = test_config();
        config.services[0].routing = Some(RoutingConfig {
            lb_strategy: None,
            sticky: None,
            rule: "Host(`api.example.com`)".to_string(),
            entrypoints: vec!["web".to_string(), "websecure".to_string()],
            tls: false,
        });

        let state = LiveState {
            containers: vec![make_container("api", 1, 0, "web1")],
        };

        let yaml = generate_dynamic_config(&config, &state);
        assert!(yaml.contains("- \"web\""));
        assert!(yaml.contains("- \"websecure\""));
        assert!(!yaml.contains("certResolver")); // tls = false
    }

    #[test]
    fn test_host_port_allocation() {
        let config = test_config();
        let state = LiveState {
            containers: vec![
                make_container("api", 1, 0, "web1"), // 9001
                make_container("api", 1, 1, "web2"), // 9002
                make_container("api", 1, 2, "web1"), // 9003
                make_container("api", 1, 3, "web2"), // 9004
            ],
        };

        let yaml = generate_dynamic_config(&config, &state);
        assert!(yaml.contains(":9001"));
        assert!(yaml.contains(":9002"));
        assert!(yaml.contains(":9003"));
        assert!(yaml.contains(":9004"));
    }

    #[test]
    fn test_routes_to_shifted_port_not_recomputed_one() {
        // The allocator shifted gen 2's range because 9003/9004 were taken by
        // something else; containers bound 9005/9006. Recomputing
        // base + (gen-1)*replicas + instance would give 9004/9005 -- wrong.
        let mut config = test_config();
        config.services[0].replicas = 2;
        let state = LiveState {
            containers: vec![
                with_port(make_container("api", 2, 0, "web1"), 9005),
                with_port(make_container("api", 2, 1, "web2"), 9006),
            ],
        };
        let yaml = generate_dynamic_config(&config, &state);
        assert!(yaml.contains("\"http://10.0.0.1:9005\""));
        assert!(yaml.contains("\"http://10.0.0.2:9006\""));
        assert!(!yaml.contains(":9004"));
    }

    #[test]
    fn test_old_generation_with_different_replicas_keeps_its_ports() {
        // Gen 1 ran with 3 replicas, gen 2 with 2; during the overlap both are routed
        // to the ports each container really bound.
        let mut config = test_config();
        config.services[0].replicas = 2;
        let state = LiveState {
            containers: vec![
                with_port(make_container("api", 1, 2, "web1"), 9003),
                with_port(make_container("api", 2, 0, "web1"), 9005),
            ],
        };
        let yaml = generate_dynamic_config(&config, &state);
        assert!(yaml.contains("http://10.0.0.1:9003"));
        assert!(yaml.contains("http://10.0.0.1:9005"));
    }

    #[test]
    fn test_container_without_known_port_is_not_routed() {
        let config = test_config();
        let mut unknown = make_container("api", 1, 1, "web2");
        unknown.host_port = None;
        let state = LiveState {
            containers: vec![make_container("api", 1, 0, "web1"), unknown],
        };
        let yaml = generate_dynamic_config(&config, &state);
        assert!(yaml.contains("http://10.0.0.1:9001"));
        assert!(!yaml.contains("10.0.0.2"));
    }

    #[test]
    fn test_rule_is_escaped_as_yaml_string() {
        let mut config = test_config();
        config.services[0].routing.as_mut().unwrap().rule =
            r#"Host(`a.com`) && PathRegexp(`^/x\d+"`)"#.to_string();
        let state = LiveState {
            containers: vec![make_container("api", 1, 0, "web1")],
        };
        let yaml = generate_dynamic_config(&config, &state);
        let rule_line = yaml
            .lines()
            .find(|l| l.trim_start().starts_with("rule:"))
            .unwrap();
        let quoted = rule_line.trim_start().trim_start_matches("rule:").trim();
        // The quoted scalar decodes back to the original rule.
        let decoded: String = serde_json::from_str(quoted).unwrap();
        assert_eq!(decoded, config.services[0].routing.as_ref().unwrap().rule);
        assert!(quoted.contains(r#"\\d"#));
        assert!(quoted.contains(r#"\""#));
    }

    #[test]
    fn test_first_host_in_rule() {
        assert_eq!(
            first_host_in_rule("Host(`api.example.com`)").as_deref(),
            Some("api.example.com")
        );
        assert_eq!(
            first_host_in_rule("PathPrefix(`/v1`) && Host( `a.example.com` , `b.example.com`)")
                .as_deref(),
            Some("a.example.com")
        );
        assert_eq!(
            first_host_in_rule("HostRegexp(`^.+\\.example\\.com$`)"),
            None
        );
        assert_eq!(first_host_in_rule("HostSNI(`*`)"), None);
        assert_eq!(first_host_in_rule("MyHost(`x`)"), None);
        assert_eq!(first_host_in_rule("PathPrefix(`/`)"), None);
        assert_eq!(first_host_in_rule("Host(``)"), None);
    }
}
