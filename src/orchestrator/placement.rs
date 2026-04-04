use crate::config::types::HostConfig;

/// Compute placement: distribute N replicas across matching hosts using round-robin.
/// Returns a vec of (host_name, instance_index) pairs.
pub fn place_replicas<'a>(hosts: &[&'a HostConfig], replicas: u32) -> Vec<(&'a HostConfig, u32)> {
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
        let mut h = HostConfig::test_host(name, &format!("10.0.0.{}", name.len()));
        h.labels = vec!["web".to_string()];
        h
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

    #[test]
    fn test_zero_replicas() {
        let h1 = make_host("web1");
        let hosts: Vec<&HostConfig> = vec![&h1];
        let placements = place_replicas(&hosts, 0);
        assert!(placements.is_empty());
    }

    #[test]
    fn test_instance_indices_sequential() {
        let h1 = make_host("web1");
        let h2 = make_host("web2");
        let hosts: Vec<&HostConfig> = vec![&h1, &h2];

        let placements = place_replicas(&hosts, 4);
        let indices: Vec<u32> = placements.iter().map(|(_, i)| *i).collect();
        assert_eq!(indices, vec![0, 1, 2, 3]);
    }

    #[test]
    fn test_three_hosts_even_distribution() {
        let h1 = make_host("a");
        let h2 = make_host("b");
        let h3 = make_host("c");
        let hosts: Vec<&HostConfig> = vec![&h1, &h2, &h3];

        let placements = place_replicas(&hosts, 6);
        assert_eq!(placements.len(), 6);
        // Each host should get exactly 2
        let a_count = placements.iter().filter(|(h, _)| h.name == "a").count();
        let b_count = placements.iter().filter(|(h, _)| h.name == "b").count();
        let c_count = placements.iter().filter(|(h, _)| h.name == "c").count();
        assert_eq!(a_count, 2);
        assert_eq!(b_count, 2);
        assert_eq!(c_count, 2);
    }

    #[test]
    fn test_three_hosts_uneven_distribution() {
        let h1 = make_host("a");
        let h2 = make_host("b");
        let h3 = make_host("c");
        let hosts: Vec<&HostConfig> = vec![&h1, &h2, &h3];

        let placements = place_replicas(&hosts, 7);
        let a_count = placements.iter().filter(|(h, _)| h.name == "a").count();
        let b_count = placements.iter().filter(|(h, _)| h.name == "b").count();
        let c_count = placements.iter().filter(|(h, _)| h.name == "c").count();
        // 7 / 3 = 2 remainder 1: first host gets extra
        assert_eq!(a_count, 3);
        assert_eq!(b_count, 2);
        assert_eq!(c_count, 2);
    }

    #[test]
    fn test_one_replica_per_host() {
        let h1 = make_host("a");
        let h2 = make_host("b");
        let h3 = make_host("c");
        let hosts: Vec<&HostConfig> = vec![&h1, &h2, &h3];

        let placements = place_replicas(&hosts, 3);
        assert_eq!(placements[0].0.name, "a");
        assert_eq!(placements[1].0.name, "b");
        assert_eq!(placements[2].0.name, "c");
    }

    #[test]
    fn test_more_hosts_than_replicas() {
        let h1 = make_host("a");
        let h2 = make_host("b");
        let h3 = make_host("c");
        let h4 = make_host("d");
        let hosts: Vec<&HostConfig> = vec![&h1, &h2, &h3, &h4];

        let placements = place_replicas(&hosts, 2);
        assert_eq!(placements.len(), 2);
        assert_eq!(placements[0].0.name, "a");
        assert_eq!(placements[1].0.name, "b");
    }
}
