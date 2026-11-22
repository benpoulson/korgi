# Health Checks

Korgi never routes traffic to a container, and never stops the containers it replaces, until the new containers have passed their health check. This page describes how containers are judged healthy.

## Modes

### `mode = "docker"` (default)

Runs a command inside the container using Docker's HEALTHCHECK mechanism. Korgi injects:

```
HEALTHCHECK --interval=5s --timeout=3s --retries=3
  CMD (curl -sf http://localhost:PORT/PATH > /dev/null) || (wget -q --spider http://localhost:PORT/PATH) || exit 1
```

Then polls `docker inspect` until the health status is `healthy`. An `unhealthy` status fails the check immediately.

**Requirements**: The container image must have a shell (`/bin/sh`) and either `curl` or `wget`.

**Best for**: Standard images based on Alpine, Debian, Ubuntu, etc.

```toml
[services.health]
path = "/health"
interval = "5s"
timeout = "3s"
retries = 3
start_period = "10s"
```

### `mode = "http"`

Korgi makes HTTP requests to the container's health endpoint from outside, via the host port. No command is executed inside the container.

The URL is `http://<internal_address>:<host_port><path>`, where `<host_port>` is the port the container actually published (recorded in its `korgi.host_port` label when it was created -- not recomputed from `host_base`). Each request uses `timeout`; requests are repeated every `interval`.

**Requirements**: The service must have `host_base` configured in `[services.ports]` so korgi can reach it from the network (validated at config load).

**Best for**: Minimal images (`FROM scratch`, distroless) with no shell.

```toml
[services.health]
mode = "http"
path = "/health"
interval = "5s"
timeout = "3s"
retries = 3
start_period = "10s"

[services.ports]
container = 8080
host_base = 9001
```

### `mode = "image"`

Keeps the `HEALTHCHECK` defined in the image (korgi does not override it) and polls Docker's health status, exactly like `docker` mode. `path` is optional; when set it is only used for Traefik's own backend health check.

If the image defines no `HEALTHCHECK`, korgi falls back to the [running check](#services-without-health-checks) below.

**Best for**: Images that ship their own, more meaningful health check.

```toml
[services.health]
mode = "image"
interval = "5s"     # used for polling and the deadline
timeout = "3s"
retries = 3
start_period = "30s"
```

## Configuration

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `path` | string | required for `docker`/`http` | HTTP path to check (e.g. `/health`, `/ready`). Must start with `/`. |
| `mode` | `"docker"` / `"http"` / `"image"` | `"docker"` | Check mode. |
| `interval` | duration | `"5s"` | Time between health checks. |
| `timeout` | duration | `"3s"` | Timeout for each check (HTTP request timeout in `http` mode). |
| `retries` | integer | `3` | Consecutive failures before Docker reports unhealthy. Also sizes the deadline. |
| `start_period` | duration | | Grace period before failures count. |

Durations use Go syntax and are parsed strictly: `500ms`, `5s`, `1m30s`, `1h`. A number without a unit (`"5"`) or an unknown unit is rejected when the config is loaded instead of silently falling back to a default.

## How health checks interact with deployment

During a deploy (and a rollback or scale-up):

1. New containers start (deploy, scale) or the rollback target is restarted (rollback)
2. Korgi checks **all** of them concurrently, under one deadline
3. If any container fails or the deadline passes, the new containers are removed (deploy, scale) or stopped again (rollback), and nothing else changes
4. Only after every container is healthy is Traefik updated; only after **every** Traefik host has confirmed the new config are the old containers stopped

The deadline is:

```
start_period + (interval + timeout) * (retries + 1) + start_delay + 15s
```

It no longer depends on `drain_seconds` (which may be `0`).

## Services without health checks

If no `[services.health]` section is present (or the image has no HEALTHCHECK in `image` mode, or Docker reports no health status), korgi requires each container to:

- be running and not restarting, and
- keep the same restart count for `start_delay` seconds (default 5).

A container that crash-loops under a restart policy (its restart count goes up) fails the check, even if it happens to be running at the moment korgi looks.

## Traefik health checks

When a service has both a health `path` and `[services.routing]`, korgi also adds a health check to the service's load balancer in the generated Traefik config:

```yaml
healthCheck:
  path: "/health"
  interval: "5s"
  timeout: "3s"
```

This lets Traefik independently exclude unhealthy backends from the load balancer pool.
