# Health Checks

Korgi supports two health check modes for verifying containers are ready to receive traffic.

## Modes

### `mode = "docker"` (default)

Runs a command inside the container using Docker's HEALTHCHECK mechanism. Korgi injects:

```
HEALTHCHECK --interval=5s --timeout=3s --retries=3
  CMD (curl -sf http://localhost:PORT/PATH > /dev/null) || (wget -q --spider http://localhost:PORT/PATH) || exit 1
```

Then polls `docker inspect` until the health status is `healthy`.

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

The URL is constructed as: `http://<internal_address>:<host_port><path>`

**Requirements**: The service must have `host_base` (or `host`) configured in `[services.ports]` so korgi can reach it from the network.

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

## Configuration

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `path` | string | required | HTTP path to check (e.g. `/health`, `/ready`, `/`). |
| `mode` | `"docker"` / `"http"` | `"docker"` | Check mode. |
| `interval` | string | `"5s"` | Time between health checks. |
| `timeout` | string | `"3s"` | Timeout for each check. |
| `retries` | integer | `3` | Number of consecutive failures before unhealthy (docker mode only). |
| `start_period` | string | | Grace period before checks count (docker mode only). |

## How health checks interact with deployment

During a deploy:

1. New containers start with Traefik labels
2. Korgi waits for all new containers to become healthy
3. If any container fails the health check, **all new containers are stopped and removed**
4. The old generation continues serving traffic (untouched)

The health check timeout during deploy is `drain_seconds * 2`.

## Services without health checks

If no `[services.health]` section is present, korgi waits `start_delay` seconds (default 5) and then checks the container is still running (hasn't crashed).

## Traefik health check labels

When a service has both `[services.health]` and `[services.routing]`, korgi also sets Traefik-side health check labels:

- `traefik.http.services.*.loadbalancer.healthcheck.path`
- `traefik.http.services.*.loadbalancer.healthcheck.interval`
- `traefik.http.services.*.loadbalancer.healthcheck.timeout`

This lets Traefik independently exclude unhealthy backends from the load balancer pool.
