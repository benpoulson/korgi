# Korgi

Docker orchestration across multiple hosts via SSH. Zero-downtime deployments, Traefik load balancing, scaling, health checks -- no agents, no daemons, just a single binary.

```
korgi deploy --service api
```

Korgi fills the gap between Docker Compose (single host) and Kubernetes (too complex). If you have 2-10 servers and want to deploy containers with zero downtime, Korgi is for you.

## Features

- **Multi-host deployments** over SSH -- no agents or daemons on your servers
- **Zero-downtime deploys** -- blue-green with automatic Traefik routing
- **Traefik integration** -- automatic load balancer configuration via Docker labels
- **Health checking** -- Docker HEALTHCHECK with automatic rollback on failure
- **Scaling** -- scale services up and down across hosts
- **Generation-based rollback** -- instant rollback to previous versions
- **Declarative config** -- define your infrastructure in a single TOML file
- **Single binary** -- no runtime dependencies (just SSH and Docker on your hosts)

## How It Works

```
Your laptop / CI                     Your servers
┌─────────────┐                      ┌─────────────┐
│             │         SSH          │   Host A    │
│   korgi     │──────────────────────│  Traefik    │
│   binary    │                      │  api-g3-0   │
│             │         SSH          │  api-g3-1   │
│             │──────────────────────├─────────────┤
│             │                      │   Host B    │
└─────────────┘                      │  Traefik    │
                                     │  api-g3-2   │
                                     │  worker-g2-0│
                                     └─────────────┘
```

Korgi SSHs into your hosts, talks to Docker via the Docker API, and manages Traefik routing through container labels. No state files -- container labels are the source of truth.

## Quick Start

### Prerequisites

- Rust toolchain (for building)
- SSH access to your target hosts
- Docker installed on target hosts
- SSH user in the `docker` group

### Install

```sh
cargo install --path .
```

### Initialize

```sh
korgi init
```

This creates a `korgi.toml` template. Edit it with your hosts and services:

```toml
[project]
name = "myapp"

[[hosts]]
name = "web1"
address = "192.168.1.10"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["web"]

[[hosts]]
name = "web2"
address = "192.168.1.11"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["web"]

[traefik]
image = "traefik:v3.2"
hosts = ["web1", "web2"]
entrypoints = { web = ":80", websecure = ":443" }
network = "korgi-traefik"

# [traefik.acme]
# email = "admin@example.com"
# storage = "/letsencrypt/acme.json"

[[services]]
name = "api"
image = "myapp/api:latest"
replicas = 3
placement_labels = ["web"]

[services.health]
path = "/health"
interval = "5s"
timeout = "3s"
retries = 3
start_period = "10s"

[services.routing]
rule = "Host(`api.example.com`)"
entrypoints = ["websecure"]
tls = true

[services.ports]
container = 8080

[services.deploy]
drain_seconds = 30
rollback_keep = 2
```

### Validate

```sh
korgi check
```

Tests SSH connectivity and Docker access on all configured hosts.

### Deploy

```sh
# Deploy Traefik first
korgi traefik deploy

# Deploy all services
korgi deploy

# Deploy a specific service
korgi deploy --service api

# Deploy with an image override (useful in CI)
korgi deploy --service api --image myapp/api:v2.1

# Preview what would happen
korgi deploy --dry-run
```

### Monitor

```sh
# See what's running where
korgi status

# JSON output for scripting
korgi status --json

# Tail logs
korgi logs --service api --follow
```

### Scale

```sh
korgi scale --service api 5
```

### Rollback

```sh
korgi rollback --service api
```

Restarts the previous generation's containers and stops the current ones.

## Commands

| Command | Description |
|---------|-------------|
| `korgi init` | Scaffold a `korgi.toml` config file |
| `korgi check` | Validate config and test SSH/Docker connectivity |
| `korgi status` | Show running containers across all hosts |
| `korgi deploy` | Zero-downtime deployment |
| `korgi rollback` | Roll back to the previous generation |
| `korgi scale` | Scale a service up or down |
| `korgi traefik deploy` | Deploy Traefik to configured hosts |
| `korgi traefik status` | Show Traefik status |
| `korgi traefik logs` | Tail Traefik logs |
| `korgi exec` | Run a command in a service container |
| `korgi logs` | Tail service logs |
| `korgi destroy` | Stop and remove containers |

All commands accept `--env <name>` (load `korgi.<name>.toml` overlay), `--config <path>`, and `--json`.

## Zero-Downtime Deploy Pipeline

```
1. PREPARE     → query live state, compute placement
2. PULL        → pull image on target hosts (parallel)
3. START GREEN → create new containers with Traefik labels
4. HEALTH CHECK → wait for Docker HEALTHCHECK to pass
   └─ on failure → stop & remove new containers, abort
5. DRAIN OLD   → gracefully stop previous generation
6. CLEANUP     → remove containers beyond rollback_keep
```

The old generation is **never touched** until the new one is confirmed healthy. If health checks fail, the new containers are removed and the old ones keep serving traffic.

## Configuration

### Environment Overlays

Create `korgi.staging.toml` with overrides, then deploy with:

```sh
korgi deploy --env staging
```

Overlays deep-merge into the base config. Tables merge recursively; arrays are replaced.

### Variable Interpolation

Reference environment variables with `${VAR}`:

```toml
[services.env]
DATABASE_URL = "${DATABASE_URL}"
```

Unset variables cause a hard error -- Korgi never deploys with empty credentials.

### Private Registries

```toml
[[registries]]
url = "ghcr.io"
username = "${GHCR_USER}"
password = "${GHCR_TOKEN}"
```

### Resource Limits

```toml
[services.resources]
memory = "512m"
cpus = "1.5"
```

### Volumes

```toml
[[services.volumes]]
host = "/data/uploads"
container = "/app/uploads"
readonly = false
```

### Services Without Routing

Background workers without a `[services.routing]` section get no Traefik labels and aren't exposed:

```toml
[[services]]
name = "worker"
image = "myapp/worker:latest"
replicas = 2
placement_labels = ["web"]
```

## State Management

Korgi has **zero local state files**. All state lives in Docker container labels:

```
korgi.project    = "myapp"
korgi.service    = "api"
korgi.generation = "4"
korgi.instance   = "0"
korgi.image      = "myapp/api:v2"
```

Every command queries Docker on all hosts for the current state. This means:
- No state drift between Korgi and reality
- Multiple engineers can run Korgi against the same infrastructure
- Crash recovery is free -- just re-run the command

## Comparison

| | Korgi | Kamal | Docker Compose | Kubernetes |
|-|-------|-------|----------------|------------|
| Multi-host | 2-10 hosts | Yes | No | Yes |
| Zero-downtime | Yes | Yes | No | Yes |
| No agents | Yes | Yes | N/A | No |
| Proxy | Traefik | kamal-proxy | N/A | Ingress |
| Scaling | Yes | Limited | No | Yes (HPA) |
| Config | TOML | YAML | YAML | YAML |
| Language | Rust | Ruby | Go | Go |
| Complexity | Low | Low | Very low | High |

## Development

```sh
# Build
cargo build

# Run tests
cargo test

# Run with debug logging
RUST_LOG=debug cargo run -- status

# Clippy
cargo clippy
```

## Architecture

See [`docs/`](docs/) for detailed documentation:

- [PLAN.md](docs/PLAN.md) -- implementation plan and project structure
- [ARCHITECTURE.md](docs/ARCHITECTURE.md) -- connection model, state management, deployment pipeline
- [DECISIONS.md](docs/DECISIONS.md) -- architecture decision records
- [DEPENDENCIES.md](docs/DEPENDENCIES.md) -- crate choices and API notes
- [EDGE-CASES.md](docs/EDGE-CASES.md) -- failure modes and recovery
- [COMPARISON.md](docs/COMPARISON.md) -- how Korgi compares to other tools
- [FUTURE.md](docs/FUTURE.md) -- planned enhancements

## License

MIT
