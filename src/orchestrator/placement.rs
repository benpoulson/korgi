use crate::config::types::HostConfig;

/// Compute placement: distribute N replicas across matching hosts using round-robin.
/// Returns a vec of (host_name, instance_index) pairs.
pub fn place_replicas<'a>(
    hosts: &[&'a HostConfig],
    replicas: u32,
) -> Vec<(&'a HostConfig, u32)> {
    if hosts.is_empty() {
        return Vec::new();
    }

    let mut placements = Vec::new();
    for i in 0..replicas {
        let host = &hosts[i as usize % hosts.len()];
        placements.push((*host, i));
    }
    placements
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::HostConfig;

    fn make_host(name: &str) -> HostConfig {
        HostConfig {
            name: name.to_string(),
            address: format!("10.0.0.{}", name.len()),
            user: "deploy".to_string(),
            ssh_key: None,
            labels: vec!["web".to_string()],
            docker_socket: None,
        }
    }

    #[test]
    fn test_round_robin_placement() {
        let h1 = make_host("web1");
        let h2 = make_host("web2");
        let hosts: Vec<&HostConfig> = vec![&h1, &h2];

        let placements = place_replicas(&hosts, 5);
        assert_eq!(placements.len(), 5);
        assert_eq!(placements[0].0.name, "web1");
        assert_eq!(placements[1].0.name, "web2");
        assert_eq!(placements[2].0.name, "web1");
        assert_eq!(placements[3].0.name, "web2");
        assert_eq!(placements[4].0.name, "web1");
    }

    #[test]
    fn test_single_host() {
        let h1 = make_host("web1");
        let hosts: Vec<&HostConfig> = vec![&h1];

        let placements = place_replicas(&hosts, 3);
        assert_eq!(placements.len(), 3);
        assert!(placements.iter().all(|(h, _)| h.name == "web1"));
    }

    #[test]
    fn test_empty_hosts() {
        let hosts: Vec<&HostConfig> = vec![];
        let placements = place_replicas(&hosts, 3);
        assert!(placements.is_empty());
    }
}
