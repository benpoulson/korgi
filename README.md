# Korgi

Docker orchestration across multiple hosts via SSH. Zero-downtime deployments, Traefik load balancing, scaling, health checks -- no agents, no daemons, just a single binary.

```
korgi deploy --service api
```

## Install

```sh
curl -sL https://raw.githubusercontent.com/benpoulson/korgi/master/install.sh | sh
```

Detects your OS and architecture automatically. Supports macOS (Intel/Apple Silicon) and Linux (x86_64/ARM64).

Or build from source: `cargo install --path .`

---

Korgi fills the gap between Docker Compose (single host) and Kubernetes (too complex). If you have 2-10 servers and want to deploy containers with zero downtime, Korgi is for you.

## Features

- **Multi-host deployments** over SSH -- no agents or daemons on your servers
- **Zero-downtime deploys** -- blue-green with Traefik sync before drain
- **Cross-host load balancing** -- dedicated Traefik entrypoint routes to containers on worker hosts
- **Sticky sessions & LB strategies** -- cookie-based affinity, round-robin or least-connections
- **Health checking** -- Docker HEALTHCHECK or HTTP polling (works with FROM scratch images)
- **Scaling** -- scale services up and down across hosts
- **Interactive service picker** -- multi-select which services to deploy
- **Generation-based rollback** -- instant rollback to previous versions
- **Declarative config** -- define your infrastructure in a single TOML file
- **Single binary** -- no runtime dependencies (just SSH and Docker on your hosts)

## Architecture

Korgi supports a dedicated load balancer host that routes traffic to containers running on internal worker hosts:

```
           Internet
              │
              ▼
        ┌───────────┐
        │  lb host   │  Public IP: 203.0.113.1
        │  Traefik   │  Runs Traefik only -- no app containers
        │  :80 :443  │  Routes via file provider to workers
        └──────┬─────┘
               │ internal network
       ┌───────┴────────┐
       ▼                ▼
 ┌───────────┐    ┌───────────┐
 │ worker-1  │    │ worker-2  │
 │ 10.0.0.10 │    │ 10.0.0.11 │
 │           │    │           │
 │ api-g3-0  │    │ api-g3-1  │
 │ :9001     │    │ :9002     │
 │ api-g3-2  │    │ api-g3-3  │
 │ :9003     │    │ :9004     │
 └───────────┘    └───────────┘
```

Korgi SSHs into all hosts, manages containers via the Docker API, and automatically generates Traefik routing config after every deploy, scale, or rollback. No state files -- container labels are the source of truth.

You can also run Traefik on every host (co-located mode) if you prefer -- the entrypoint/worker split is optional.

## Quick Start

### Prerequisites

- SSH access to your target hosts (key-based auth, passphrases supported)
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

# --- Load balancer (runs Traefik, faces the internet) ---
[[hosts]]
name = "lb"
role = "lb"                        # runs Traefik -- no app containers
address = "203.0.113.1"            # public IP (SSH connects here)
internal_address = "10.0.0.1"      # private IP (Traefik routes via this)
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"

# --- Worker nodes (run containers, internal only) ---
[[hosts]]
name = "worker-1"
address = "10.0.0.10"
internal_address = "10.0.0.10"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["app"]

[[hosts]]
name = "worker-2"
address = "10.0.0.11"
internal_address = "10.0.0.11"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["app"]

[traefik]
image = "traefik:v3.2"
entrypoints = { web = ":80", websecure = ":443" }
network = "korgi-traefik"

[traefik.acme]
email = "admin@example.com"
storage = "/letsencrypt/acme.json"

[[services]]
name = "api"
image = "myapp/api:latest"
replicas = 4
placement_labels = ["app"]         # Only placed on worker hosts

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
host_base = 9001                   # Workers expose 9001, 9002, ... for Traefik

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
# Deploy Traefik first (generates routing config automatically)
korgi traefik deploy

# Deploy all services
korgi deploy

# Deploy a specific service with an image override (useful in CI)
korgi deploy --service api --image myapp/api:v2.1

# Preview what would happen
korgi deploy --dry-run
```

After each deploy, Korgi automatically syncs the Traefik routing config with the new container topology.

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
korgi scale --service api 8
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
1. PREPARE       → query live state, compute placement, find free ports
2. PULL          → pull image on target hosts
3. START GREEN   → create new containers with host port bindings
4. HEALTH CHECK  → wait for containers to become healthy
   └─ failure    → stop & remove new containers, abort
5. SYNC CONFIG   → update Traefik to route to both old + new (zero-downtime overlap)
6. DRAIN OLD     → gracefully stop ALL old generation containers
7. SYNC CONFIG   → update Traefik to remove drained containers
8. CLEANUP       → remove containers beyond rollback_keep
```

The old generation is **never stopped** until Traefik has been updated to route traffic to the new containers (step 5). After draining, Traefik is synced again (step 7) to remove stale URLs. If health checks fail, the new containers are removed and the old ones keep serving traffic.

## Cross-Host Load Balancing

Korgi uses two mechanisms for Traefik routing:

- **Docker provider** -- Traefik discovers containers on its own host via the Docker socket
- **File provider** -- Korgi generates a dynamic YAML config listing all backends across all hosts by `internal_ip:host_port`, and writes it into the Traefik container

After every `deploy`, `scale`, `rollback`, or `destroy`, Korgi regenerates the config:

```yaml
# Generated by korgi -- do not edit manually
http:
  routers:
    myapp-api:
      rule: "Host(`api.example.com`)"
      service: myapp-api
      entryPoints:
        - websecure
      tls:
        certResolver: letsencrypt
  services:
    myapp-api:
      loadBalancer:
        servers:
          - url: "http://10.0.0.10:9001"
          - url: "http://10.0.0.11:9002"
          - url: "http://10.0.0.10:9003"
          - url: "http://10.0.0.11:9004"
        healthCheck:
          path: /health
          interval: 5s
          timeout: 3s
```

Traefik watches this file for changes and updates routing automatically.

### Host Roles

Every host has a `role` -- either `lb` (load balancer) or `node` (default):

```toml
[[hosts]]
name = "lb"
role = "lb"           # runs Traefik, faces the internet
address = "203.0.113.1"

[[hosts]]
name = "worker-1"
# role = "node"       # default -- runs containers
address = "10.0.0.10"
labels = ["app"]
```

- `role = "lb"` -- Traefik is deployed here automatically. No app containers unless it also has matching placement labels.
- `role = "node"` (default) -- runs application containers. Traefik is not deployed here.

The `[traefik]` section no longer needs a `hosts` field -- Korgi automatically deploys Traefik to all `role = "lb"` hosts.

### Host Addresses

Each host has two addresses:

```toml
[[hosts]]
name = "worker-1"
address = "203.0.113.10"           # public -- used for SSH connections
internal_address = "10.0.0.10"     # private -- used for Traefik routing
port = 22                          # SSH port (default: 22)
```

If `internal_address` is not set, `address` is used for both SSH and routing.

### SSH Authentication

Korgi uses pure Rust SSH (no system `ssh` binary needed). Authentication methods:

- **Key file** -- set `ssh_key` on the host. Passphrase-protected keys are supported (you'll be prompted).
- **Default keys** -- if no `ssh_key` is set, Korgi tries `~/.ssh/id_ed25519`, `~/.ssh/id_rsa`, `~/.ssh/id_ecdsa` in order.
- **Passphrases** -- if a key is encrypted, Korgi prompts for the passphrase interactively.

### Port Allocation

Containers bind to host ports so Traefik can reach them across the network:

```toml
[services.ports]
container = 8080       # port inside the container
host_base = 9001       # instance 0 → 9001, instance 1 → 9002, ...
```

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

Unset variables cause a hard error -- Korgi never deploys with empty credentials. Variables in TOML comments are ignored.

### Secrets File

Keep sensitive values out of your environment by pointing to a secrets file:

```toml
[project]
name = "myapp"
secrets = ".korgi-secrets"
```

The file uses `KEY=VALUE` format (comments with `#`, blank lines ignored):

```
DB_PASSWORD=hunter2
JWT_SECRET=supersecret
GH_TOKEN=ghp_abc123
```

The file is optional -- if it doesn't exist, Korgi falls back to system env vars. System env always takes precedence over the secrets file. Add the file to your `.gitignore`.

### Private Registries

GitHub Container Registry shorthand:

```toml
[[registries]]
github_token = "${GH_TOKEN}"
```

Other registries:

```toml
[[registries]]
url = "registry.example.com"
username = "${REGISTRY_USER}"
password = "${REGISTRY_PASSWORD}"
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

Background workers without a `[services.routing]` section get no Traefik config and aren't exposed:

```toml
[[services]]
name = "worker"
image = "myapp/worker:latest"
replicas = 2
placement_labels = ["app"]
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
| Cross-host LB | Yes | No | No | Yes |
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

# Run tests (217 unit tests, no Docker needed)
cargo test

# Run with debug logging
RUST_LOG=debug cargo run -- status

# Clippy
cargo clippy

# Integration tests (requires Docker)
cd tests/integration
./setup.sh       # start 2 DinD hosts with SSH
./run_tests.sh   # full lifecycle test
./teardown.sh    # clean up
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
