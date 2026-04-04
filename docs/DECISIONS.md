# Korgi -- Architecture Decision Records

## ADR-001: Use bollard's built-in SSH instead of custom transport

**Status:** Accepted  
**Date:** 2026-04-04

### Context
The original plan proposed building a custom `DockerSshTransport` that opens russh `direct-streamlocal` channels to `/var/run/docker.sock`, then bridges them to bollard via hyper HTTP/1.1 handshake. This was identified as the "hardest piece" of the project.

### Research Findings
bollard v0.20 has a built-in `ssh` feature that provides `Docker::connect_with_ssh()`. Under the hood, it uses the `openssh` crate which spawns an SSH subprocess and tunnels Docker API calls through it.

### Decision
Use bollard's built-in SSH support instead of building a custom transport.

### Consequences
- **Removed:** `ssh/transport.rs`, `ssh/channel_io.rs` -- no longer needed
- **Simplified:** DockerHost is ~20 lines instead of ~200
- **Dependency:** relies on system SSH binary being available (always true on Linux/macOS)
- **Trade-off:** spawns a subprocess per host connection (acceptable for 2-10 hosts)
- **Kept russh:** still needed for `korgi check` SSH connectivity tests and `korgi exec`

---

## ADR-002: Traefik over kamal-proxy

**Status:** Accepted  
**Date:** 2026-04-04

### Context
Kamal (Ruby-based deployment tool by 37signals) uses `kamal-proxy` -- a purpose-built reverse proxy. We considered both options.

### Decision
Use Traefik as the reverse proxy/load balancer.

### Rationale
- **Routing flexibility:** Traefik supports complex routing rules (host, path, headers, regex), weighted routing, middleware (rate limiting, auth, circuit breakers)
- **ACME/Let's Encrypt:** built-in automatic TLS certificate management
- **Docker provider:** native Docker label-based service discovery -- no config files to manage
- **TCP routing:** supports non-HTTP protocols for services like Redis, PostgreSQL
- **Dashboard:** optional web UI for debugging routing
- **Ecosystem:** huge community, extensive documentation, battle-tested
- **kamal-proxy** is simpler but tightly coupled to Kamal's deployment model

### Consequences
- Each host runs its own Traefik instance (Docker provider is local-only)
- External DNS/LB needed to distribute across Traefik instances
- More config surface area than kamal-proxy
- Traefik's Docker network sharing requirement must be managed by korgi

---

## ADR-003: No local state files -- labels are truth

**Status:** Accepted  
**Date:** 2026-04-04

### Context
Deployment tools typically maintain state in one of three ways:
1. Local state file (Terraform, Pulumi)
2. Remote state store (Terraform with S3 backend)
3. Infrastructure labels/tags as state (Kubernetes labels, Docker labels)

### Decision
All state is derived from Docker container labels queried at runtime. Zero local state files.

### Rationale
- **No drift:** state always reflects reality
- **Multi-user:** multiple engineers can run korgi without state conflicts
- **Crash-safe:** interrupted deploys leave labeled containers that korgi can discover
- **Simple:** no state file format, no locking, no backup needed

### Trade-offs
- Every command queries all hosts (acceptable for 2-10 hosts, ~100ms per host)
- No deployment history (would need separate audit logging)
- Concurrent deploys can race on generation numbers (see ADR-006)

---

## ADR-004: Health checks via Docker HEALTHCHECK, not korgi-side HTTP probing

**Status:** Accepted  
**Date:** 2026-04-04

### Context
The original plan included `reqwest` for HTTP health check polling from the local machine. This has problems:
- korgi runs locally, containers run remotely -- need SSH tunneling for HTTP access
- Container IPs are in Docker bridge networks, not routable from outside
- Adds latency (local → SSH → container instead of Docker → container)

### Decision
Use Docker's native HEALTHCHECK mechanism. Korgi injects a HEALTHCHECK config into each container and polls `docker inspect` via bollard to check `.State.Health.Status`.

### Consequences
- **Removed:** `reqwest` dependency
- Health checks run inside the container (wget/curl to localhost)
- Korgi only polls Docker for the result -- no direct HTTP access needed
- Services without health checks use a simple `start_delay` timer
- Docker must have wget or curl available inside the container image (most images do)

---

## ADR-005: Rollback via stopped containers + image labels

**Status:** Accepted  
**Date:** 2026-04-04

### Context
When rolling back, we need to restart the previous version. The plan keeps N old generations as stopped containers, but Docker may garbage-collect the underlying images.

### Decision
1. Store the image reference in a container label (`korgi.image=myapp/api:v2`)
2. Before rollback, verify the image exists on the host (`docker image inspect`)
3. If missing, re-pull it before starting the old containers

### Consequences
- Rollback is reliable even after `docker image prune`
- Extra label per container (negligible overhead)
- Rollback may be slow if image needs re-pulling

### Future Enhancement
Pin images for rollback-eligible generations with a korgi-specific tag (`korgi-keep:myapp-api-g3`) to prevent pruning. Remove the tag when the generation ages out of `rollback_keep`.

---

## ADR-006: No advisory locking for concurrent deploys

**Status:** Accepted (with known limitation)  
**Date:** 2026-04-04

### Context
Two users running `korgi deploy` simultaneously for the same service will race on generation numbers. Both will query state, see the same current generation, and try to create generation N+1.

### Decision
For v1, do not implement advisory locking. Document the limitation.

### Rationale
- Korgi targets small teams (2-10 hosts implies small team)
- CI/CD pipelines naturally serialize deployments
- Adding locking (via a Docker container, file, or external service) adds significant complexity
- The failure mode is recoverable: duplicate generation containers are detectable and cleanable

### Future Enhancement
Add advisory locking via a short-lived Docker container on a designated host:
```
korgi-lock-{project}-{service} (running = locked)
```
Check for lock before deploying, create lock container, deploy, remove lock.

---

## ADR-007: Inter-service networking is out of scope

**Status:** Accepted  
**Date:** 2026-04-04

### Context
When services span multiple hosts (api on host A, redis on host B), how do they communicate? Options:
1. Docker overlay network (requires Swarm mode)
2. WireGuard/Tailscale mesh VPN
3. Traefik TCP routing (internal entrypoint)
4. Host networking + explicit addresses

### Decision
Korgi does not manage cross-host private networking. Services on the same host share a Docker bridge network. For cross-host communication, users should:
- Use the public Traefik route (for HTTP services)
- Use a mesh VPN (WireGuard, Tailscale) if private cross-host communication is needed
- Use managed external services (AWS RDS, managed Redis, etc.)

### Rationale
- Overlay networking requires Docker Swarm (defeats korgi's purpose)
- VPN setup is infrastructure-specific and outside korgi's scope
- korgi targets the 2-10 host sweet spot where services either co-locate or use external services
- Adding networking management would 3x the project's complexity

### Documentation
Users should be clearly told: "For cross-host service communication, use Traefik routes, a mesh VPN, or external managed services. Korgi does not provide overlay networking."

---

## ADR-008: TOML over YAML for configuration

**Status:** Accepted  
**Date:** 2026-04-04

### Context
Kubernetes and Docker Compose use YAML. Ansible uses YAML. Most infra tools use YAML or HCL.

### Decision
Use TOML for korgi configuration.

### Rationale
- Rust ecosystem convention (Cargo.toml, etc.)
- No indentation-sensitivity (YAML's biggest footgun)
- Clean syntax for the nested-but-not-deeply-nested structure of deployment configs
- Better type safety (strings vs numbers vs booleans are explicit)
- serde + toml crate has excellent error messages for parse failures
- Environment overlays work naturally with TOML deep merge

### Consequences
- Users familiar with YAML need to learn TOML (minimal learning curve)
- Array-of-tables syntax (`[[services]]`) is less intuitive than YAML lists
- Docker Compose users can't copy-paste configs (but the concepts map cleanly)
