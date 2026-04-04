# Korgi vs Other Deployment Tools

## Feature Comparison Matrix

| Feature | Korgi | Kamal | Docker Compose | Docker Swarm | Kubernetes |
|---------|-------|-------|----------------|--------------|------------|
| Multi-host | Yes (2-10) | Yes | No (single host) | Yes | Yes |
| Zero-downtime deploy | Yes | Yes | No | Yes | Yes |
| No agents/daemons | Yes | Yes | N/A | No (Swarm manager) | No (kubelet, etc.) |
| Reverse proxy | Traefik | kamal-proxy | N/A | Ingress | Ingress |
| Health checks | Docker HEALTHCHECK | kamal-proxy HTTP | Docker HEALTHCHECK | Docker HEALTHCHECK | Liveness/Readiness |
| Scaling | Yes | Limited | No | Yes | Yes (HPA) |
| Rollback | Yes (generation-based) | Yes | No | Yes | Yes |
| Config format | TOML | YAML | YAML | YAML | YAML |
| Language | Rust (single binary) | Ruby (gem) | Go (Docker CLI) | Go (Docker) | Go |
| SSH-based | Yes | Yes | No | No | No |
| State storage | Docker labels | Docker + proxy | Docker | Raft consensus | etcd |
| ACME/TLS | Traefik built-in | kamal-proxy built-in | External | External | cert-manager |
| Service mesh | No | No | No | Limited | Istio/Linkerd |
| Overlay networking | No | No | No | Yes | Yes |

## Korgi vs Kamal (closest comparison)

### How Kamal Deploys (observed from real deployment logs)

```
1. SSH into host, create .kamal directory
2. docker -v (verify Docker)
3. docker login (registry auth)
4. docker image rm --force (clean old image)
5. docker pull <image>
6. docker inspect -f '{{ .Config.Labels.service }}' (verify label)
7. docker network create kamal
8. docker container start kamal-proxy || docker run kamal-proxy
9. docker ps --filter label=service=X (detect stale containers)
10. docker rename (rename existing container)
11. docker run --detach --name X-web-latest (start new container)
12. docker exec kamal-proxy kamal-proxy deploy X --target=... --health-check-path=...
13. kamal-proxy does health checking and traffic switching
14. docker ps -q -a | tail -n +6 | xargs docker rm (prune old)
15. docker image prune (clean images)
```

### Key Differences

| Aspect | Korgi | Kamal |
|--------|-------|-------|
| **Docker interaction** | Docker API via bollard | `docker` CLI commands via SSH exec |
| **Proxy** | Traefik (rich routing rules, middleware, TCP) | kamal-proxy (simple HTTP-only) |
| **Health checking** | Docker HEALTHCHECK + bollard inspect | kamal-proxy HTTP probe |
| **Container versioning** | Generation-based (g1, g2, g3...) | Single "latest" + renames |
| **Multi-service** | Native (services array in config) | One service per deploy.yml (accessories separate) |
| **Scaling** | `korgi scale --service X 5` | Not a first-class operation |
| **Rollback** | Generation-based (restart stopped gen) | Image-version based |
| **Labels** | Rich (project, service, generation, instance, image) | Minimal (service, role, destination) |
| **Network** | Manages Docker networks | Creates simple "kamal" network |
| **State** | Derived from labels, stateless | Derived from container names + proxy state |

### When to Choose Korgi over Kamal

- You want Traefik's routing flexibility (path-based, header-based, TCP)
- You need to scale services up/down dynamically
- You manage multiple services per project as a unit
- You want generation-based rollback (not just image version rollback)
- You prefer TOML config and Rust tooling
- You want a single static binary (no Ruby runtime)

### When to Choose Kamal over Korgi

- You're already in the Ruby/Rails ecosystem
- You want the simplest possible setup (kamal-proxy is simpler than Traefik)
- You need `kamal build` for image building (korgi doesn't build)
- You want a mature, production-tested tool (Kamal has years of production use)
- You need accessories management (databases, Redis as managed containers)

## Korgi vs Docker Compose

Docker Compose is single-host only. Korgi extends the Compose mental model to multiple hosts:

| Compose concept | Korgi equivalent |
|-----------------|------------------|
| `services:` | `[[services]]` |
| `docker-compose up` | `korgi deploy` |
| `docker-compose down` | `korgi destroy` |
| `docker-compose ps` | `korgi status` |
| `docker-compose logs` | `korgi logs` |
| `docker-compose exec` | `korgi exec` |
| `replicas: 3` | `replicas = 3` (across hosts) |
| `deploy.labels` | `[services.routing]` |
| volumes/networks | `[[services.volumes]]`, managed networks |

Key additions korgi provides over Compose:
- Multi-host deployment
- Zero-downtime rolling deploys
- Generation-based rollback
- SSH-based remote management
- Automatic Traefik configuration

## Korgi vs Docker Swarm

Swarm provides overlay networking and built-in orchestration, but requires Swarm mode initialization and manager nodes. Korgi targets users who:

- Don't want to run Swarm managers
- Have 2-10 hosts (Swarm's Raft consensus is overkill)
- Want direct SSH control over each host
- Prefer explicit placement over Swarm's scheduler
- Don't need overlay networking (see ADR-007)

## Korgi vs Kubernetes

Kubernetes is for large-scale deployments (10+ nodes, microservices). Korgi is for the "too big for one server, too small for Kubernetes" sweet spot:

- No etcd, no API server, no kubelet, no scheduler
- No YAML manifests, no Helm charts
- No service mesh complexity
- Just SSH + Docker + Traefik
- Single binary, instant setup
