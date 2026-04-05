# Korgi

Docker orchestration across multiple hosts via SSH. Zero-downtime deployments, Traefik load balancing, scaling, health checks -- no agents, no daemons, just a single binary.

## Quick Links

- **[Installation](Installation)**
- **[Getting Started](Getting-Started)**
- **[Configuration Reference](Configuration-Reference)**
- **[CLI Reference](CLI-Reference)**
- **[Health Checks](Health-Checks)**
- **[Cross-Host Load Balancing](Cross-Host-Load-Balancing)**
- **[Deployment Pipeline](Deployment-Pipeline)**
- **[SSH Authentication](SSH-Authentication)**
- **[Secrets Management](Secrets-Management)**
- **[Troubleshooting](Troubleshooting)**

## Architecture

**Single server** (`role = "both"`):

```
        +-----------+
        |  server   |  role = "both"
        |  Traefik  |  Traefik + containers on one machine
        |  :80 :443 |
        |           |
        | api-g3-0  |
        | api-g3-1  |
        | worker-0  |
        +-----------+
```

**Multi-server** (dedicated LB + workers):

```
        +-----------+
        |  lb host   |  role = "lb"
        |  Traefik   |  Routes to workers via file provider
        |  :80 :443  |
        +------+-----+
               |
       +-------+--------+
       v                v
 +-----------+    +-----------+
 | worker-1  |    | worker-2  |
 | 10.0.0.10 |    | 10.0.0.11 |
 | api-g3-0  |    | api-g3-1  |
 | :9001     |    | :9002     |
 +-----------+    +-----------+
```
