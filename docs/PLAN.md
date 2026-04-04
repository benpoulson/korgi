# Korgi — Docker Orchestration Tool

## Context

Build a Rust CLI tool that manages Docker containers across 2-10 hosts via SSH, with Traefik load balancing, zero-downtime deployments, scaling, and health checking. Think "Ansible meets Docker Compose for multi-host" — no central server, no agents, just a single binary that SSHs into hosts and talks to Docker.

---

## Tech Stack

| Component | Choice | Rationale |
|-----------|--------|-----------|
| SSH | `russh` | Pure Rust, async/tokio, no C deps, supports `direct-streamlocal` for Docker socket |
| Docker | `bollard` (with `ssh` feature) | Most maintained Docker API crate, async, built-in SSH transport via `openssh` |
| CLI | `clap` (derive) | Industry standard, derive macros for clean code |
| Config | `toml` + `serde` | Rust ecosystem convention, clean syntax for infra config |
| Async | `tokio` | Required by both russh and bollard |
| Errors | `thiserror` + `anyhow` | `thiserror` for library errors, `anyhow` for CLI |
| Output | `tabled` + `indicatif` + `console` | Tables for status, progress bars for deploys, colored output |
| Logging | `tracing` + `tracing-subscriber` | Structured logging for debugging SSH/Docker issues across hosts |

**Not included:** `reqwest` — health checks use Docker's native HEALTHCHECK via bollard inspect, not HTTP polling from the local machine.

---

## Config Schema (`korgi.toml`)

```toml
[project]
name = "myapp"

[[registries]]                      # optional, for private images
url = "ghcr.io"
username = "${GHCR_USER}"
password = "${GHCR_TOKEN}"

[[hosts]]
name = "web1"
address = "192.168.1.10"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["web", "primary"]
# docker_socket = "/var/run/docker.sock"  # optional override

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
network = "korgi-traefik"           # shared network Traefik + services join

[traefik.acme]                      # optional Let's Encrypt
email = "admin@example.com"
storage = "/letsencrypt/acme.json"

[[services]]
name = "api"
image = "myapp/api:latest"
replicas = 3
placement_labels = ["web"]          # spread across hosts with these labels
command = []                        # optional CMD override
entrypoint = []                     # optional ENTRYPOINT override
restart = "unless-stopped"          # default

[services.health]
path = "/health"
interval = "5s"
timeout = "3s"
retries = 3
start_period = "10s"                # grace period before health checks count

[services.routing]
rule = "Host(`api.example.com`)"
entrypoints = ["websecure"]
tls = true

[services.env]
DATABASE_URL = "${DATABASE_URL}"
REDIS_URL = "redis://redis:6379"

[services.ports]
container = 8080
# host = 8080                       # optional: expose on host port (rare)

[[services.volumes]]
host = "/data/uploads"
container = "/app/uploads"
readonly = false

[services.resources]
memory = "512m"
cpus = "1.0"

[services.deploy]
drain_seconds = 30
start_delay = 5                     # used when no health check defined
rollback_keep = 2                   # keep N old generations for rollback

[[services]]
name = "worker"
image = "myapp/worker:latest"
replicas = 2
placement_labels = ["web"]
# no routing section = no Traefik labels (background worker)
# no health section = uses start_delay instead
```

Environment overlays (`korgi.staging.toml`) deep-merge over base config with `--env staging`.

---

## CLI Commands

```
korgi init                              # scaffold korgi.toml
korgi check                             # validate config + test SSH/Docker to all hosts
korgi status [--service <name>] [--json] # show what's running where
korgi deploy [--service <name>] [--image <override>] [--dry-run]
korgi rollback --service <name>          # restart previous generation
korgi scale --service <name> <count>     # adjust replicas
korgi traefik deploy|status|logs         # manage Traefik itself
korgi exec --service <name> -- <cmd...>  # run command in container
korgi logs --service <name> [--follow]   # tail logs
korgi destroy [--service <name>]         # stop and remove
```

All commands accept `--env <name>`, `--config <path>`, and `--json`.

---

## Project Structure

```
korgi/
  Cargo.toml
  src/
    main.rs                     # entry point, clap parse, dispatch
    lib.rs                      # re-exports for testability
    cli/
      mod.rs                    # clap derive structs
      output.rs                 # table formatting, JSON, progress bars
    config/
      mod.rs                    # load_config() with merge + interpolation
      types.rs                  # serde structs for korgi.toml
      merge.rs                  # environment overlay deep-merge
      interpolate.rs            # ${VAR} expansion
    ssh/
      mod.rs
      session.rs                # SshSession wrapper around russh
      pool.rs                   # SshPool — one connection per host
    docker/
      mod.rs
      host.rs                   # DockerHost — bollard client per host (connect_with_ssh)
      containers.rs             # container config builder, KorgiContainer type
      labels.rs                 # Traefik + korgi label generation
      registry.rs               # private registry auth
    orchestrator/
      mod.rs
      state.rs                  # LiveState — query containers across hosts
      placement.rs              # spread replicas across matching hosts
      deploy.rs                 # zero-downtime deployment pipeline
      rollback.rs
      scale.rs
    health/
      mod.rs
      checker.rs                # poll Docker HEALTHCHECK status via bollard inspect
    commands/
      mod.rs                    # one handler per CLI command
      init.rs
      check.rs
      status.rs
      deploy.rs
      rollback.rs
      scale.rs
      traefik.rs
      exec.rs
      logs.rs
      destroy.rs
```

---

## Core Architecture

### SSH + Docker Connection Model

```
korgi binary (local)
  └── DockerHost per host (bollard via connect_with_ssh)
       ├── web1: Docker::connect_with_ssh("ssh://deploy@192.168.1.10")
       │    └── bollard API calls → openssh subprocess → Docker socket
       ├── web2: Docker::connect_with_ssh("ssh://deploy@192.168.1.11")
       └── ...
```

**Connection approach:** bollard's built-in `connect_with_ssh()` uses the `openssh` crate, which spawns an SSH subprocess and tunnels the Docker API through it. This is simpler and more robust than a custom `direct-streamlocal` transport.

russh is used separately for SSH command execution (`korgi exec`, `korgi check` connectivity tests) — not for Docker API transport.

### State Management — No Local State File

All state is derived from Docker labels on running/stopped containers:
- `korgi.project=myapp`
- `korgi.service=api`
- `korgi.generation=4`
- `korgi.instance=0`
- `korgi.image=myapp/api:v2`

Every command queries hosts for containers matching `korgi.project=<name>`. Generation number = `max(existing) + 1`. This eliminates state drift entirely.

### Container Naming

```
korgi-{project}-{service}-g{generation}-{instance}
```
Example: `korgi-myapp-api-g4-0`, `korgi-myapp-api-g4-1`

---

## Zero-Downtime Deployment Pipeline

```
deploy(service):
  1. PREPARE
     - Load config, connect Docker via SSH, query live state
     - Compute placement (round-robin across matching hosts)
     - Determine next generation number

  2. PULL (parallel across hosts)
     - docker pull <new_image> on each target host
     - Auth with private registry if configured
     - Abort all on failure

  3. ENSURE NETWORK + START GREEN (parallel)
     - Ensure korgi network exists on target hosts
     - Create containers with traefik.enable=true + routing labels
     - Docker HEALTHCHECK defined on container (if configured)
     - Traefik sees new containers but won't route until healthy
     - Both old + new may serve traffic briefly (both healthy = fine)

  4. HEALTH CHECK (parallel, with timeout)
     - Poll docker inspect → .State.Health.Status == "healthy"
     - On timeout/failure → stop + remove all green containers, abort
     - Old containers untouched (still serving)
     - If no health check configured: wait start_delay seconds instead

  5. DRAIN OLD
     - docker stop -t {drain_seconds} on old generation
     - Docker sends SIGTERM, waits, then SIGKILL
     - Traefik auto-removes stopped containers from rotation

  6. CLEANUP
     - Remove containers from generations older than current - rollback_keep
     - Keep recent stopped generations for rollback
```

**Rollback**: Find most recent stopped generation → verify image exists (re-pull if garbage-collected) → `docker start` those containers (labels still intact) → stop current generation.

**Error boundaries**: Each phase gates on the previous. Green-phase failures clean up green containers only. The old generation is never touched until green is confirmed healthy.

---

## Implementation Phases

### Phase 1: Foundation
1. `cargo init`, set up `Cargo.toml` with dependencies
2. Config module — `types.rs` (serde structs), `merge.rs`, `interpolate.rs`
3. CLI skeleton — clap derive structs, subcommand dispatch
4. `korgi init` command — scaffold a template `korgi.toml`
5. Unit tests: config parsing, merge, interpolation

### Phase 2: SSH
6. `SshSession` — connect via russh, `exec()` for shell commands
7. `SshPool` — manage connections to all hosts
8. `korgi check` — validate config + test SSH connectivity

### Phase 3: Docker over SSH
9. `DockerHost` — bollard client via `Docker::connect_with_ssh()`
10. Label generation (`labels.rs`)
11. Container CRUD operations (`containers.rs`)
12. `LiveState` — query containers across all hosts
13. `korgi status` — display what's running where

### Phase 4: Traefik
14. `korgi traefik deploy` — deploy Traefik containers to configured hosts
15. `korgi traefik status` / `korgi traefik logs`

### Phase 5: Deployment Pipeline
16. Placement algorithm
17. Deploy pipeline (pull → start → health check → drain → cleanup)
18. `korgi deploy` command with `--dry-run` support
19. Health check polling

### Phase 6: Operations
20. `korgi rollback`
21. `korgi scale`
22. `korgi exec`
23. `korgi logs`
24. `korgi destroy`

### Phase 7: Polish
25. Progress bars, colored output, `--json` mode
26. Structured logging with `tracing`
27. Error messages and edge cases

---

## Verification

- **Unit tests**: Config parsing, label generation, placement algorithm, config merge, interpolation
- **Integration tests**: Use a local Docker instance to test container lifecycle
- **Manual testing**: Set up 2 VMs/cloud instances, deploy a sample app, verify zero-downtime with `curl` loop during deploy
- **Key scenarios to test**:
  - `korgi check` validates SSH + Docker connectivity
  - Deploy → verify Traefik routes traffic
  - Deploy new version → verify zero dropped requests
  - Health check failure during deploy → verify rollback
  - `korgi scale` up/down → verify container count
  - `korgi rollback` → verify previous generation restarts
  - SSH connection failure mid-deploy → verify partial state is recoverable
  - Service without health check → deploys with start_delay
