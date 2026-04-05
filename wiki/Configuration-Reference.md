# Configuration Reference

Korgi is configured via `korgi.toml`. All fields documented below.

## `[project]`

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `name` | string | yes | | Project name. Used in container names and labels. |
| `secrets` | string | no | | Path to a secrets file (KEY=VALUE format). Resolved relative to the config file. |

```toml
[project]
name = "myapp"
secrets = ".korgi-secrets"
```

## `[[hosts]]`

Define one or more hosts. Each host is either a load balancer or a worker node.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `name` | string | yes | | Unique host identifier. |
| `role` | `"lb"` / `"node"` | no | `"node"` | `lb` runs Traefik. `node` runs containers. |
| `address` | string | yes | | SSH address (public/external IP or hostname). |
| `internal_address` | string | no | same as `address` | Internal IP for Traefik routing and inter-host traffic. |
| `user` | string | no | `"root"` | SSH username. |
| `port` | integer | no | `22` | SSH port. |
| `ssh_key` | string | no | auto-detect | Path to SSH private key. If not set, tries `~/.ssh/id_ed25519`, `id_rsa`, `id_ecdsa`. |
| `labels` | list of strings | no | `[]` | Labels for placement targeting. |
| `docker_socket` | string | no | `/var/run/docker.sock` | Path to Docker socket on the host. |

```toml
[[hosts]]
name = "lb"
role = "lb"
address = "203.0.113.1"
internal_address = "10.0.0.1"
user = "deploy"
port = 22
ssh_key = "~/.ssh/id_ed25519"

[[hosts]]
name = "worker-1"
address = "10.0.0.10"
internal_address = "10.0.0.10"
user = "deploy"
labels = ["app", "gpu"]
```

### Host roles

- **`role = "lb"`** -- Traefik is deployed here automatically. No app containers unless it also has matching placement labels.
- **`role = "node"`** (default) -- runs application containers. Traefik is not deployed here.

## `[traefik]`

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `image` | string | no | `"traefik:v3.2"` | Traefik Docker image. |
| `entrypoints` | table | no | `{}` | Map of entrypoint name to listen address. |
| `network` | string | no | `"korgi-traefik"` | Docker network shared by Traefik and services. |
| `hosts` | list of strings | no | auto from `role = "lb"` | Explicit list of hosts to run Traefik on. Overrides role-based detection. |

```toml
[traefik]
image = "traefik:v3.2"
entrypoints = { web = ":80", websecure = ":443" }
network = "korgi-traefik"
```

### `[traefik.acme]`

Optional. Enables automatic TLS via Let's Encrypt.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `email` | string | yes | | Email for Let's Encrypt account. |
| `storage` | string | no | `"/letsencrypt/acme.json"` | Path inside the Traefik container. |

When ACME is configured with both `web` and `websecure` entrypoints, HTTP-to-HTTPS redirect is enabled automatically.

## `[[registries]]`

Define private container registries for image pulls.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `github_token` | string | | | Shorthand for GHCR. Sets url=ghcr.io, username=token automatically. |
| `url` | string | | | Registry URL (e.g. `ghcr.io`, `registry.example.com`). |
| `username` | string | | | Registry username. |
| `password` | string | | | Registry password/token. |

GitHub Container Registry shorthand:

```toml
[[registries]]
github_token = "${GH_TOKEN}"
```

Other registries:

```toml
[[registries]]
url = "registry.example.com"
username = "${REG_USER}"
password = "${REG_PASS}"
```

## `[[services]]`

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `name` | string | yes | | Service name. Must be unique. |
| `image` | string | yes | | Docker image reference. |
| `replicas` | integer | no | `1` | Number of container instances. |
| `placement_labels` | list of strings | no | `[]` | Only place on hosts with ALL these labels. Empty = all hosts. |
| `command` | list of strings | no | | Override container CMD. |
| `entrypoint` | list of strings | no | | Override container ENTRYPOINT. |
| `restart` | string | no | `"unless-stopped"` | Restart policy: `no`, `always`, `on-failure`, `unless-stopped`. |

```toml
[[services]]
name = "api"
image = "myapp/api:v1"
replicas = 3
placement_labels = ["app"]
command = ["serve", "--port", "8080"]
restart = "unless-stopped"
```

### `[services.health]`

Health check configuration. See [Health Checks](Health-Checks) for full details.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `path` | string | yes | | HTTP path to check (e.g. `/health`). |
| `mode` | `"docker"` / `"http"` | no | `"docker"` | `docker` runs a command inside the container. `http` has korgi poll externally. |
| `interval` | string | no | `"5s"` | Time between checks. |
| `timeout` | string | no | `"3s"` | Timeout per check. |
| `retries` | integer | no | `3` | Failures before unhealthy. |
| `start_period` | string | no | | Grace period before checks count. |

### `[services.routing]`

Traefik routing rules. Services without routing are background workers (no Traefik config).

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `rule` | string | yes | | Traefik routing rule (e.g. `` Host(`api.example.com`) ``). |
| `entrypoints` | list of strings | no | `[]` | Which Traefik entrypoints to listen on. |
| `tls` | boolean | no | `false` | Enable TLS with Let's Encrypt cert resolver. |

### `[services.ports]`

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `container` | integer | yes | | Port inside the container. |
| `host` | integer | no | | Fixed host port (only for single replica per host). |
| `host_base` | integer | no | | Base port for auto-allocation. Instance 0 gets `host_base`, instance 1 gets `host_base+1`, etc. Required for cross-host routing. |

### `[services.env]`

Environment variables passed to the container. Supports `${VAR}` interpolation from the secrets file and system environment.

```toml
[services.env]
DATABASE_URL = "${DATABASE_URL}"
REDIS_URL = "redis://10.0.0.1:6379"
```

### `[[services.volumes]]`

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `host` | string | yes | | Host path. |
| `container` | string | yes | | Container path. |
| `readonly` | boolean | no | `false` | Mount as read-only. |

### `[services.resources]`

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `memory` | string | no | | Memory limit (e.g. `512m`, `1g`). |
| `cpus` | string | no | | CPU limit (e.g. `1.0`, `0.5`). |

### `[services.deploy]`

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `drain_seconds` | integer | no | `30` | Grace period for stopping old containers (SIGTERM timeout). |
| `start_delay` | integer | no | `5` | Seconds to wait if no health check is configured. |
| `rollback_keep` | integer | no | `2` | Number of old generations to keep for rollback. |

## Environment Overlays

Create `korgi.<env>.toml` for per-environment overrides:

```sh
korgi deploy --env staging  # loads korgi.staging.toml overlay
```

Overlays deep-merge into the base config. Tables merge recursively; arrays are replaced.

## Variable Interpolation

`${VAR}` references are resolved from the secrets file and system environment. System env takes precedence. Unset variables cause a hard error. Variables in TOML comments are ignored.
