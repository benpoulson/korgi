# Development Guide

## Project Overview

Korgi is a Rust CLI tool for orchestrating Docker containers across 2-10 hosts via SSH. It manages Traefik load balancing, zero-downtime deployments, scaling, health checking, and rollback. No central server, no agents -- just SSH + Docker API.

The repo is named `kargo` but the binary/tool is named `korgi`.

**Primary pattern**: A dedicated Traefik entrypoint host (internet-facing load balancer) routes to containers running on internal worker hosts. The entrypoint host doesn't need to run app containers -- it can be purely a proxy.

## Build & Test

```sh
cargo build           # build
cargo test            # run all 224 unit tests
cargo clippy          # lint
cargo fmt             # format
RUST_LOG=debug cargo run -- status  # run with debug logging
```

Tests run fast (~5s) -- no Docker or SSH needed. Orchestrator tests use `MockDockerHost` from `src/docker/mock.rs`.

### Integration Tests

```sh
cd tests/integration
./setup.sh       # generate SSH keys, start 2 DinD hosts
./run_tests.sh   # full lifecycle: check → traefik → deploy → scale → rollback → destroy
./teardown.sh    # clean up
```

## Architecture

Single crate, `lib.rs` + `main.rs`. Seven module groups:

```
src/
  main.rs              # tokio entry, clap dispatch
  lib.rs               # re-exports all modules
  cli/                 # clap derive structs, output formatting (tables, spinners, colors)
  config/              # TOML parsing, env overlay merge, ${VAR} interpolation, validation
  ssh/                 # russh session wrapper, connection pool (for `check` and `exec`)
  docker/              # bollard client over SSH, container CRUD, label generation, registry auth
    traits.rs          # DockerHostApi trait -- all orchestrator code is generic over this
    mock.rs            # MockDockerHost for testing (cfg(test) only)
  orchestrator/        # deployment pipeline, rollback, scaling, live state queries, placement
    traefik_config.rs  # generates Traefik file-provider YAML for cross-host routing
  health/              # polls Docker HEALTHCHECK status via docker inspect
  commands/            # one handler per CLI command (thin dispatch to orchestrator)
    sync_config.rs     # pushes generated Traefik config into Traefik container after changes
```

### Key Design Decisions

- **No local state files** -- all state derived from Docker container labels at runtime
- **Host roles** -- `role = "lb"` (runs Traefik) or `role = "node"` (default, runs containers). `Config::traefik_host_names()` auto-derives from role, with backwards-compat for explicit `[traefik].hosts`
- **Cross-host routing via Traefik file provider** -- after deploy/scale/rollback/destroy, Korgi generates a dynamic YAML config and writes it into the Traefik container via `docker exec`. Traefik watches the file and updates routing automatically
- **Host port binding** -- containers bind to host ports (`host_base + instance`) on the internal IP so Traefik can route to them across the network
- **Public vs internal addresses** -- `address` is for SSH, `internal_address` is for Traefik routing. Falls back to `address` if `internal_address` is not set
- **bollard `connect_with_ssh()`** for Docker API -- not a custom SSH transport
- **russh** used separately for SSH command exec (`check`, `exec` commands)
- **Docker HEALTHCHECK** polled via `docker inspect`, not HTTP probing from local machine
- **Generation-based versioning** -- containers tagged with monotonic generation numbers for rollback

### Deploy Pipeline

```
PREPARE → PULL → START GREEN → HEALTH CHECK → DRAIN OLD → CLEANUP → SYNC TRAEFIK CONFIG
```

Old generation is never touched until new is confirmed healthy. Health failure triggers cleanup of new containers only. Traefik config is synced after every topology change.

### Traefik Config Sync

`commands/sync_config.rs` runs after deploy/scale/rollback/destroy:
1. Queries live state across all hosts
2. `orchestrator/traefik_config.rs` generates YAML mapping services → `internal_ip:host_port` backends
3. Writes the YAML into the Traefik container at `/etc/korgi/dynamic.yml` via `docker exec`
4. Traefik's file provider watches and picks up changes

## Config Format

`korgi.toml` -- TOML with `[[hosts]]`, `[[services]]`, `[traefik]`.

- **Hosts**: `role` (`lb`/`node`, default `node`), `address` (SSH), `internal_address` (routing), `port` (SSH port, default 22), `labels` (placement targeting)
- **Services**: `placement_labels` controls which hosts get containers. `host_base` in `[services.ports]` allocates sequential host ports for cross-host routing
- **Traefik**: `hosts` lists which hosts run Traefik (typically just the LB). File provider is enabled automatically
- **Overlays**: `korgi.<env>.toml` deep-merged over base via `--env`
- **Interpolation**: `${VAR}` from system environment (strict -- unset vars are errors, comment lines skipped)
- **Validation**: duplicate service names rejected, placement labels must match at least one host, traefik host refs validated

## Conventions

- Error handling: `anyhow::Result` everywhere, `context()` for error messages
- Async: tokio runtime, all Docker/SSH operations are async
- Logging: `tracing` crate -- `debug!` for operational detail, `info!` for milestones
- CLI output: `cli/output.rs` helpers -- `success()`, `error()`, `warn()`, `info()`, `spinner()`, `progress_bar()`
- Container names: `korgi-{project}-{service}-g{generation}-{instance}`
- Labels: `korgi.project`, `korgi.service`, `korgi.generation`, `korgi.instance`, `korgi.image`
- Traefik labels: health check path/interval/timeout set on containers with routing + health config
- Tests: unit tests in each module's `#[cfg(test)] mod tests`, orchestrator tests use `MockDockerHost`
- Test helpers: `HostConfig::test_host()`, `ServiceConfig::test_service()` for building minimal configs
- The `DockerHostApi` trait in `docker/traits.rs` abstracts Docker operations -- orchestrator functions are generic over it
- Rust edition 2024 -- `std::env::set_var`/`remove_var` require `unsafe`
