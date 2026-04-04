# Kargo — Docker Orchestration Tool

## Context

Build a Rust CLI tool that manages Docker containers across 2-10 hosts via SSH, with Traefik load balancing, zero-downtime deployments, scaling, and health checking. Think "Ansible meets Docker Compose for multi-host" — no central server, no agents, just a single binary that SSHs into hosts and talks to Docker.

---

## Tech Stack

| Component | Choice | Rationale |
|-----------|--------|-----------|
| SSH | `russh` | Pure Rust, async/tokio, no C deps, supports `direct-streamlocal` for Docker socket |
| Docker | `bollard` | Most maintained Docker API crate, async, supports custom transports |
| CLI | `clap` (derive) | Industry standard, derive macros for clean code |
| Config | `toml` + `serde` | Rust ecosystem convention, clean syntax for infra config |
| Async | `tokio` | Required by both russh and bollard |
| HTTP (health) | `reqwest` | For HTTP health check polling |
| Errors | `thiserror` + `anyhow` | `thiserror` for library errors, `anyhow` for CLI |
| Output | `tabled` + `indicatif` | Tables for status, progress bars for deploys |

---

## Config Schema (`kargo.toml`)

```toml
[project]
name = "myapp"

[[hosts]]
name = "web1"
address = "192.168.1.10"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["web", "primary"]

[[hosts]]
name = "web2"
address = "192.168.1.11"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["web"]

[traefik]
image = "traefik:v3.2"
hosts = ["web1", "web2"]           # which hosts run Traefik
entrypoints = { web = ":80", websecure = ":443" }

[traefik.acme]                     # optional Let's Encrypt
email = "admin@example.com"
storage = "/letsencrypt/acme.json"

[[services]]
name = "api"
image = "myapp/api:latest"
replicas = 3
placement_labels = ["web"]         # spread across hosts with these labels

[services.health]
path = "/health"
interval = "5s"
timeout = "3s"
retries = 3

[services.routing]
rule = "Host(`api.example.com`)"
entrypoints = ["websecure"]
tls = true

[services.env]
DATABASE_URL = "${DATABASE_URL}"
REDIS_URL = "redis://redis:6379"

[services.ports]
container = 8080

[services.deploy]
drain_seconds = 30
rollback_keep = 2                  # keep N old generations for rollback

[[services]]
name = "worker"
image = "myapp/worker:latest"
replicas = 2
placement_labels = ["web"]
# no routing section = no Traefik labels (background worker)
```

Environment overlays (`kargo.staging.toml`) deep-merge over base config with `--env staging`.

---

## CLI Commands

```
kargo init                              # scaffold kargo.toml
kargo check                             # validate config + test SSH to all hosts
kargo status [--service <name>] [--json] # show what's running where
kargo deploy [--service <name>] [--image <override>] [--dry-run]
kargo rollback --service <name>          # restart previous generation
kargo scale --service <name> <count>     # adjust replicas
kargo traefik deploy|status|logs         # manage Traefik itself
kargo exec --service <name> -- <cmd...>  # run command in container
kargo logs --service <name> [--follow]   # tail logs
kargo destroy [--service <name>]         # stop and remove
```

All commands accept `--env <name>` and `--config <path>`.

---

## Project Structure

```
kargo/
  Cargo.toml
  src/
    main.rs                     # entry point, clap parse, dispatch
    lib.rs                      # re-exports for testability
    cli/
      mod.rs                    # clap derive structs
      output.rs                 # table formatting, JSON, progress bars
    config/
      mod.rs
      types.rs                  # serde structs for kargo.toml
      merge.rs                  # environment overlay deep-merge
      interpolate.rs            # ${VAR} expansion
    ssh/
      mod.rs
      session.rs                # SshSession wrapper around russh
      pool.rs                   # SshPool — one connection per host
      transport.rs              # DockerSshTransport (critical piece)
    docker/
      mod.rs
      host.rs                   # DockerHost — bollard client per host
      containers.rs             # container CRUD operations
      labels.rs                 # Traefik + kargo label generation
    orchestrator/
      mod.rs
      state.rs                  # LiveState — query containers across hosts
      placement.rs              # spread replicas across matching hosts
      deploy.rs                 # zero-downtime deployment pipeline
      rollback.rs
      scale.rs
    health/
      mod.rs
      checker.rs                # poll Docker HEALTHCHECK status
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
kargo binary (local)
  └── SshPool (HashMap<host_name, SshSession>)
       ├── web1: SshSession (one russh TCP connection)
       │    └── DockerHost (bollard via DockerSshTransport)
       │         └── API call → channel_open_direct_streamlocal("/var/run/docker.sock")
       │              → HTTP/1.1 over SSH channel → dockerd
       ├── web2: SshSession → DockerHost
       └── web3: SshSession → DockerHost
```

**`DockerSshTransport`** is the critical bridge: implements bollard's custom transport by opening an SSH `direct-streamlocal` channel to `/var/run/docker.sock` on the remote host, then doing HTTP/1.1 via `hyper::client::conn::http1::handshake` over that channel. No TCP ports, no `socat`, no local sockets.

### State Management — No Local State File

All state is derived from Docker labels on running/stopped containers:
- `kargo.project=myapp`
- `kargo.service=api`
- `kargo.generation=4`
- `kargo.instance=0`

Every command queries hosts for containers matching `kargo.project=<name>`. Generation number = `max(existing) + 1`. This eliminates state drift entirely.

### Container Naming

```
kargo-{project}-{service}-g{generation}-{instance}
```
Example: `kargo-myapp-api-g4-0`, `kargo-myapp-api-g4-1`

---

## Zero-Downtime Deployment Pipeline

```
deploy(service):
  1. PREPARE
     - Load config, connect SSH, query live state
     - Compute placement (round-robin across matching hosts)
     - Determine next generation number
  
  2. PULL (parallel across hosts)
     - docker pull <new_image> on each target host
     - Abort all on failure
  
  3. START GREEN (parallel)
     - Create containers with traefik.enable=true + routing labels
     - Docker HEALTHCHECK defined on container
     - Traefik sees new containers but won't route until healthy
     - Both old + new may serve traffic briefly (both healthy = fine)
  
  4. HEALTH CHECK (parallel, with timeout)
     - Poll docker inspect → .State.Health.Status == "healthy"
     - On timeout/failure → stop + remove all green containers, abort
     - Old containers untouched (still serving)
  
  5. DRAIN OLD
     - docker stop -t {drain_seconds} on old generation
     - Docker sends SIGTERM, waits, then SIGKILL
     - Traefik auto-removes stopped containers
  
  6. CLEANUP
     - Remove containers from generations older than current - rollback_keep
     - Keep recent stopped generations for rollback
```

**Rollback**: Find most recent stopped generation → `docker start` those containers (labels still intact) → health check → stop current generation.

**Error boundaries**: Each phase gates on the previous. Green-phase failures clean up green containers only. The old generation is never touched until green is confirmed healthy.

---

## Implementation Order

### Phase 1: Foundation
1. `cargo init`, set up `Cargo.toml` with dependencies
2. Config module — `types.rs` (serde structs), `merge.rs`, `interpolate.rs`
3. CLI skeleton — clap derive structs, subcommand dispatch
4. `kargo init` command — scaffold a template `kargo.toml`

### Phase 2: SSH
5. `SshSession` — connect via russh, `exec()` for shell commands
6. `SshPool` — manage connections to all hosts
7. `kargo check` — validate config + test SSH connectivity

### Phase 3: Docker over SSH (hardest piece)
8. `DockerSshTransport` — bridge russh channels to bollard
9. `DockerHost` — bollard client per host
10. Label generation (`labels.rs`)
11. `kargo status` — query containers across all hosts

### Phase 4: Traefik
12. `kargo traefik deploy` — deploy Traefik containers to configured hosts
13. `kargo traefik status` / `kargo traefik logs`

### Phase 5: Deployment Pipeline
14. Placement algorithm
15. Deploy pipeline (pull → start → health check → drain → cleanup)
16. `kargo deploy` command
17. Health check polling

### Phase 6: Operations
18. `kargo rollback`
19. `kargo scale`
20. `kargo exec`
21. `kargo logs`
22. `kargo destroy`

### Phase 7: Polish
23. Progress bars, colored output, `--json` mode
24. `--dry-run` support
25. Error messages and edge cases

---

## Verification

- **Unit tests**: Config parsing, label generation, placement algorithm, config merge
- **Integration tests**: Use a local Docker instance to test container lifecycle
- **Manual testing**: Set up 2 VMs/cloud instances, deploy a sample app, verify zero-downtime with `curl` loop during deploy
- **Key scenarios to test**:
  - Deploy → verify Traefik routes traffic
  - Deploy new version → verify zero dropped requests
  - Health check failure during deploy → verify rollback
  - `kargo scale` up/down → verify container count
  - `kargo rollback` → verify previous generation restarts
  - SSH connection failure mid-deploy → verify partial state is recoverable
