# Korgi -- Architecture Deep Dive

## Overview

Korgi is a single-binary deployment tool that manages Docker containers across 2-10 hosts via SSH. No central server, no agents, no daemons -- just SSH + Docker API.

```
┌─────────────────┐
│  korgi binary    │  (your laptop / CI runner)
│  (local)         │
└─────┬───────────┘
      │ SSH
      ├───────────────────────────────┐
      ▼                               ▼
┌─────────────┐                ┌─────────────┐
│   Host A    │                │   Host B    │
│             │                │             │
│  ┌────────┐ │                │  ┌────────┐ │
│  │Traefik │ │                │  │Traefik │ │
│  │ :80,:443│ │                │  │ :80,:443│ │
│  └───┬────┘ │                │  └───┬────┘ │
│      │      │                │      │      │
│  ┌───▼────┐ │                │  ┌───▼────┐ │
│  │api-g3-0│ │                │  │api-g3-1│ │
│  │api-g3-2│ │                │  │worker-0│ │
│  │worker-1│ │                │  └────────┘ │
│  └────────┘ │                │             │
└─────────────┘                └─────────────┘
```

## Connection Model

### Docker API over SSH (bollard + openssh)

Korgi uses bollard's built-in `Docker::connect_with_ssh()` which leverages the `openssh` crate. Under the hood, this spawns an SSH subprocess that tunnels HTTP requests to the remote Docker daemon's Unix socket.

```rust
let client = Docker::connect_with_ssh(
    "ssh://deploy@192.168.1.10",  // SSH URL
    120,                           // timeout seconds
    bollard::API_DEFAULT_VERSION,  // Docker API version
    Some("/path/to/key".into()),   // optional SSH key path
)?;
```

**Why this approach over a custom transport:**
- bollard's SSH support handles all the HTTP/Unix socket bridging internally
- No need to write a `ChannelStream` adapter (AsyncRead+AsyncWrite over russh channels)
- The `openssh` crate reuses the system's SSH configuration (~/.ssh/config, known_hosts, etc.)
- Simpler error handling -- SSH failures surface as bollard errors

**Why we still have russh:**
- `korgi check` tests SSH connectivity independently of Docker
- `korgi exec` can fall back to SSH exec if needed
- Future: direct `docker` CLI execution over SSH as a fallback transport

### Alternative approaches considered

| Approach | Pros | Cons | Verdict |
|----------|------|------|---------|
| **bollard connect_with_ssh** | Simple, maintained, works | Spawns SSH subprocess per connection | **Chosen** |
| **russh direct-streamlocal** | Pure Rust, no subprocess | 80-100 lines of ChannelStream adapter code, harder to debug | Rejected for v1 |
| **SSH local port forward** | Simple bollard connection | Requires Docker TCP listener on remote (security risk) | Rejected |
| **SSH exec docker CLI** | Simplest | No structured API, fragile parsing, no streaming, slow | Rejected |
| **SSH exec socat** | Moderate complexity | Requires socat on remote host | Possible fallback |

## State Model

### No Local State -- Labels Are Truth

Korgi has **zero local state files**. All state is derived from Docker container labels queried at runtime:

```
korgi.project   = "myapp"       # which korgi project owns this
korgi.service   = "api"         # service name from config
korgi.generation = "4"          # deployment generation (monotonic)
korgi.instance  = "0"           # instance index within generation
korgi.image     = "myapp/api:v2" # image used to create this container
```

**Benefits:**
- No state drift between korgi's view and reality
- Multiple machines can run korgi against the same infrastructure
- Containers are self-describing -- `docker inspect` shows all korgi metadata
- Crash recovery is free -- just re-run the command

**Trade-offs:**
- Every command does a full state query across all hosts (acceptable for 2-10 hosts)
- No history of past deployments (add audit logging in a future version)
- Concurrent deploys can race on generation numbers (see Edge Cases)

### Container Naming Convention

```
korgi-{project}-{service}-g{generation}-{instance}
```

Examples:
- `korgi-myapp-api-g4-0` -- service "api", 4th deployment, first instance
- `korgi-myapp-api-g4-1` -- same deployment, second instance
- `korgi-myapp-worker-g3-0` -- service "worker", 3rd deployment

### Generation Lifecycle

```
Generation 1: Created → Running → Stopped (drained by gen 2)
Generation 2: Created → Running → Stopped (drained by gen 3)
Generation 3: Created → Running (current)
                    ↑
                    └── korgi deploy creates generation 4
                        gen 3 drained after gen 4 is healthy
```

Old generations are kept stopped for `rollback_keep` deployments, then removed.

## Traefik Integration

### Per-Host Model

Each host runs its own Traefik instance. Traefik's Docker provider discovers containers on the **local** Docker daemon only -- it cannot see containers on other hosts.

```
Host A:                          Host B:
┌──────────┐                     ┌──────────┐
│ Traefik  │ discovers           │ Traefik  │ discovers
│          │──── api-g4-0        │          │──── api-g4-1
│          │──── api-g4-2        │          │──── worker-g3-0
└──────────┘                     └──────────┘
```

**How traffic reaches the right host:** DNS (round-robin A records) or an external load balancer points at all Traefik hosts. Each Traefik instance only routes to local containers, so traffic is naturally distributed.

### Label-Based Routing

Korgi generates Traefik labels on containers:

```
traefik.enable = "true"
traefik.http.routers.myapp-api.rule = "Host(`api.example.com`)"
traefik.http.routers.myapp-api.entrypoints = "websecure"
traefik.http.routers.myapp-api.tls = "true"
traefik.http.routers.myapp-api.tls.certresolver = "letsencrypt"
traefik.http.services.myapp-api.loadbalancer.server.port = "8080"
traefik.docker.network = "korgi-traefik"
```

Services without a `[routing]` section (e.g., background workers) get no Traefik labels and are not exposed.

### Network Requirements

Traefik and application containers must share a Docker network. Korgi creates and manages this:

1. `korgi traefik deploy` creates the `korgi-traefik` network on each host
2. `korgi deploy` attaches service containers to this network
3. Traefik routes traffic within the shared network

## Deployment Pipeline Detail

### Phase Flow

```
  PREPARE ─── fail ──→ abort (no changes made)
     │
     ▼
   PULL ───── fail ──→ abort (images pulled but no containers)
     │
     ▼
  START GREEN ─ fail → cleanup green containers, abort
     │
     ▼
  HEALTH CHECK ─ fail → cleanup green containers, abort
     │                   (old generation untouched, still serving)
     ▼
  DRAIN OLD ──── fail → old containers may be partially stopped
     │                   (green is serving, recoverable)
     ▼
  CLEANUP ───── fail ──→ old containers not cleaned up (harmless)
     │
     ▼
   DONE
```

### Error Boundaries

The critical invariant: **the old generation is never touched until the new generation is confirmed healthy.**

- If green containers fail to start → remove them, old generation keeps serving
- If health checks time out → remove green, old generation keeps serving
- If draining old fails → green is already serving, partial drain is recoverable
- If cleanup fails → stale stopped containers remain (harmless, cleaned up next deploy)

### Health Check Strategies

**With Docker HEALTHCHECK (recommended):**
```toml
[services.health]
path = "/health"
interval = "5s"
timeout = "3s"
retries = 3
start_period = "10s"
```

Korgi injects a Docker HEALTHCHECK into the container config:
```
HEALTHCHECK --interval=5s --timeout=3s --retries=3 --start-period=10s \
  CMD wget -q --spider http://localhost:8080/health || exit 1
```

Then polls `docker inspect` until `.State.Health.Status == "healthy"`.

**Without health check (background workers):**
```toml
[services.deploy]
start_delay = 5   # seconds to wait after starting
```

Korgi waits `start_delay` seconds, then checks the container is still running (hasn't crashed).

## Placement Algorithm

### Round-Robin Spread

Replicas are distributed across matching hosts using simple round-robin:

```
3 replicas, 2 hosts:
  instance 0 → host A
  instance 1 → host B
  instance 2 → host A

5 replicas, 3 hosts:
  instance 0 → host A
  instance 1 → host B
  instance 2 → host C
  instance 3 → host A
  instance 4 → host B
```

Host matching is based on `placement_labels` -- only hosts with all required labels are eligible.

### Future: Additional Strategies

Not yet implemented but the placement module is designed for extension:
- **Pinned**: explicit host assignment for stateful services
- **Least-loaded**: place on host with fewest running containers
- **Weighted**: hosts with different capacities get proportional placement

## Config System

### Loading Pipeline

```
korgi.toml ──→ merge overlay ──→ interpolate ${VAR} ──→ parse to Config ──→ validate
                    │
            korgi.staging.toml
            (if --env staging)
```

### Deep Merge

Environment overlays use recursive table merge. Arrays are replaced, not appended:

```toml
# korgi.toml (base)
[[services]]
name = "api"
image = "api:v1"
replicas = 2

# korgi.staging.toml (overlay)
[[services]]
name = "api"
image = "api:v1-staging"
replicas = 1
```

Result: staging gets 1 replica of api:v1-staging.

### Variable Interpolation

`${VAR}` references are resolved from the system environment of the machine running korgi. This works well with CI/CD (GitHub Actions secrets, GitLab CI variables).

- `${VAR}` → resolved from environment
- `$VAR` → NOT interpolated (literal text)
- `${}` → error (empty variable name)
- `${UNSET}` → error (strict -- never deploys with empty credentials)
